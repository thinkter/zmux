//! Bounded background metadata used by the workspace rail.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};

const MAX_GIT_OUTPUT_BYTES: u64 = 64 * 1024;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum MetadataState<T> {
    #[default]
    NotRequested,
    Pending,
    Ready(T),
    Unavailable(String),
    Error(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitMetadata {
    pub branch: String,
    pub dirty_files: usize,
    pub ahead: usize,
    pub behind: usize,
}

impl GitMetadata {
    pub fn compact_label(&self) -> String {
        let mut label = self.branch.clone();
        if self.dirty_files > 0 {
            label.push_str(&format!(" · {} modified", self.dirty_files));
        }
        if self.ahead > 0 {
            label.push_str(&format!(" ↑{}", self.ahead));
        }
        if self.behind > 0 {
            label.push_str(&format!(" ↓{}", self.behind));
        }
        label
    }
}

pub fn collect_git_metadata(repository: &Path) -> MetadataState<GitMetadata> {
    let mut child = match Command::new("git")
        .args([
            "-c",
            "core.quotepath=false",
            "status",
            "--porcelain=v1",
            "--branch",
            "--untracked-files=normal",
        ])
        .current_dir(repository)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return MetadataState::Unavailable("git is not installed".into());
        }
        Err(error) => return MetadataState::Error(error.to_string()),
    };

    let mut output = Vec::new();
    let read = child
        .stdout
        .take()
        .expect("piped git stdout")
        .take(MAX_GIT_OUTPUT_BYTES + 1)
        .read_to_end(&mut output);
    if let Err(error) = read {
        let _ = child.kill();
        let _ = child.wait();
        return MetadataState::Error(error.to_string());
    }
    if output.len() as u64 > MAX_GIT_OUTPUT_BYTES {
        let _ = child.kill();
        let _ = child.wait();
        return MetadataState::Error("git status output exceeded 64 KiB".into());
    }
    let status = match child.wait() {
        Ok(status) if status.success() => status,
        Ok(_) => return MetadataState::Unavailable("not a Git repository".into()),
        Err(error) => return MetadataState::Error(error.to_string()),
    };
    let _ = status;

    match parse_git_status(&String::from_utf8_lossy(&output)) {
        Some(metadata) => MetadataState::Ready(metadata),
        None => MetadataState::Error("git returned an invalid status summary".into()),
    }
}

fn parse_git_status(output: &str) -> Option<GitMetadata> {
    let mut lines = output.lines();
    let header = lines.next()?.strip_prefix("## ")?;
    let branch = if let Some(branch) = header.strip_prefix("No commits yet on ") {
        branch.split_whitespace().next()?.to_string()
    } else if header.starts_with("HEAD (no branch)") {
        "detached".to_string()
    } else {
        header
            .split_once("...")
            .or_else(|| header.split_once(" ["))
            .map_or(header, |(branch, _)| branch)
            .trim()
            .to_string()
    };
    let ahead = parse_counter(header, "ahead ");
    let behind = parse_counter(header, "behind ");
    Some(GitMetadata {
        branch,
        dirty_files: lines.count(),
        ahead,
        behind,
    })
}

fn parse_counter(header: &str, marker: &str) -> usize {
    header
        .split(marker)
        .nth(1)
        .and_then(|tail| {
            tail.chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
                .parse()
                .ok()
        })
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_branch_dirty_and_tracking_counts() {
        let metadata = parse_git_status(
            "## main...origin/main [ahead 2, behind 1]\n M src/main.rs\nM  Cargo.toml\n",
        )
        .unwrap();
        assert_eq!(metadata.branch, "main");
        assert_eq!(metadata.dirty_files, 2);
        assert_eq!(metadata.ahead, 2);
        assert_eq!(metadata.behind, 1);
    }

    #[test]
    fn parses_new_and_detached_repositories() {
        assert_eq!(
            parse_git_status("## No commits yet on trunk\n")
                .unwrap()
                .branch,
            "trunk"
        );
        assert_eq!(
            parse_git_status("## HEAD (no branch)\n").unwrap().branch,
            "detached"
        );
    }
}
