//! Git metadata projected from Zed snapshots or lightweight porcelain probes.

use std::path::{Path, PathBuf};

use async_process::Command;
use git::status::FileStatus;
use project::git_store::RepositorySnapshot;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum MetadataState<T> {
    #[default]
    NotRequested,
    Ready(T),
    Unavailable(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitMetadata {
    pub branch: String,
    pub dirty_files: usize,
    pub ahead: usize,
    pub behind: usize,
    pub added_lines: usize,
    pub deleted_lines: usize,
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

/// Project the repository data needed by the workspace rail without launching
/// another Git process. `None` means that Zed has not produced a snapshot for
/// the selected repository (or the repository has since been removed).
pub fn git_metadata_from_repository(
    repository: Option<&RepositorySnapshot>,
) -> MetadataState<GitMetadata> {
    let Some(repository) = repository else {
        return MetadataState::Unavailable("repository snapshot unavailable".into());
    };

    let branch = repository.branch.as_ref();
    let tracking = branch.and_then(|branch| branch.tracking_status());
    MetadataState::Ready(project_git_metadata(
        branch.map(|branch| branch.name()),
        tracking.map(|tracking| (tracking.ahead, tracking.behind)),
        repository.status().map(|entry| {
            (
                entry.status,
                entry.diff_stat.map(|stat| (stat.added, stat.deleted)),
            )
        }),
    ))
}

/// Read the compact rail metadata without adding the repository to Zed's
/// project model. The caller is responsible for running this future off the UI
/// executor and bounding its lifetime.
pub async fn probe_git_metadata(root: &Path) -> Result<GitMetadata, String> {
    let top_level = run_git(root, &["rev-parse", "--show-toplevel"]).await?;
    let reported_root = PathBuf::from(String::from_utf8_lossy(&top_level).trim());
    let expected_root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let reported_root = std::fs::canonicalize(&reported_root).unwrap_or(reported_root);
    if reported_root != expected_root {
        return Err(format!(
            "git reported repository root {} instead of {}",
            reported_root.display(),
            expected_root.display()
        ));
    }

    let status = run_git(
        root,
        &[
            "status",
            "--porcelain=v2",
            "--branch",
            "-z",
            "--untracked-files=normal",
        ],
    )
    .await?;
    parse_porcelain_v2(&status)
}

async fn run_git(root: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("LC_ALL", "C")
        .kill_on_drop(true);
    let output = command
        .output()
        .await
        .map_err(|error| format!("failed to launch git: {error}"))?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        Err(if stderr.is_empty() {
            "git command failed".to_string()
        } else {
            stderr.to_string()
        })
    }
}

fn parse_porcelain_v2(output: &[u8]) -> Result<GitMetadata, String> {
    let mut branch = None;
    let mut ahead = 0usize;
    let mut behind = 0usize;
    let mut dirty_files = 0usize;
    let mut skip_rename_source = false;

    for record in output
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
    {
        if skip_rename_source {
            skip_rename_source = false;
            continue;
        }
        if let Some(value) = record.strip_prefix(b"# branch.head ") {
            branch = Some(if value == b"(detached)" {
                "detached".to_string()
            } else {
                String::from_utf8_lossy(value).into_owned()
            });
            continue;
        }
        if let Some(value) = record.strip_prefix(b"# branch.ab ") {
            let value = String::from_utf8_lossy(value);
            for count in value.split_ascii_whitespace() {
                if let Some(count) = count.strip_prefix('+') {
                    ahead = count.parse().unwrap_or_default();
                } else if let Some(count) = count.strip_prefix('-') {
                    behind = count.parse().unwrap_or_default();
                }
            }
            continue;
        }
        match record.first().copied() {
            Some(b'1' | b'u' | b'?') => dirty_files = dirty_files.saturating_add(1),
            Some(b'2') => {
                dirty_files = dirty_files.saturating_add(1);
                skip_rename_source = true;
            }
            Some(b'!' | b'#') => {}
            Some(kind) => return Err(format!("unknown porcelain-v2 record type {kind:#x}")),
            None => {}
        }
    }

    Ok(GitMetadata {
        branch: branch.unwrap_or_else(|| "detached".to_string()),
        dirty_files,
        ahead,
        behind,
        added_lines: 0,
        deleted_lines: 0,
    })
}

fn project_git_metadata(
    branch: Option<&str>,
    tracking: Option<(u32, u32)>,
    statuses: impl IntoIterator<Item = (FileStatus, Option<(u32, u32)>)>,
) -> GitMetadata {
    let (ahead, behind) = tracking.unwrap_or_default();
    let mut dirty_files = 0usize;
    let mut added_lines = 0usize;
    let mut deleted_lines = 0usize;

    for (status, diff_stat) in statuses {
        // Ignored paths are not part of `git status --porcelain` and should not
        // make an otherwise clean repository appear dirty in the rail.
        if status == FileStatus::Ignored {
            continue;
        }
        dirty_files = dirty_files.saturating_add(1);
        if let Some((added, deleted)) = diff_stat {
            added_lines = added_lines.saturating_add(added as usize);
            deleted_lines = deleted_lines.saturating_add(deleted as usize);
        }
    }

    GitMetadata {
        branch: branch.unwrap_or("detached").to_string(),
        dirty_files,
        ahead: ahead as usize,
        behind: behind as usize,
        added_lines,
        deleted_lines,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use git::status::{StatusCode, TrackedStatus};

    fn modified() -> FileStatus {
        FileStatus::Tracked(TrackedStatus {
            index_status: StatusCode::Unmodified,
            worktree_status: StatusCode::Modified,
        })
    }

    #[test]
    fn projects_clean_repository() {
        assert_eq!(
            project_git_metadata("main".into(), None, []),
            GitMetadata {
                branch: "main".into(),
                dirty_files: 0,
                ahead: 0,
                behind: 0,
                added_lines: 0,
                deleted_lines: 0,
            }
        );
    }

    #[test]
    fn projects_dirty_repository_and_aggregate_diff_stats() {
        assert_eq!(
            project_git_metadata(
                Some("feature"),
                None,
                [
                    (modified(), Some((8, 4))),
                    (FileStatus::Untracked, Some((7, 2))),
                    (FileStatus::Ignored, Some((100, 100))),
                ],
            ),
            GitMetadata {
                branch: "feature".into(),
                dirty_files: 2,
                ahead: 0,
                behind: 0,
                added_lines: 15,
                deleted_lines: 6,
            }
        );
    }

    #[test]
    fn projects_detached_head_and_tracking_counts() {
        let metadata = project_git_metadata(None, Some((2, 1)), []);
        assert_eq!(metadata.branch, "detached");
        assert_eq!((metadata.ahead, metadata.behind), (2, 1));
    }

    #[test]
    fn binary_status_is_dirty_without_line_counts() {
        let metadata = project_git_metadata(Some("main"), None, [(modified(), None)]);
        assert_eq!(metadata.dirty_files, 1);
        assert_eq!((metadata.added_lines, metadata.deleted_lines), (0, 0));
    }

    #[test]
    fn unavailable_repository_has_explicit_state() {
        assert_eq!(
            git_metadata_from_repository(None),
            MetadataState::Unavailable("repository snapshot unavailable".into())
        );
    }

    #[test]
    fn parses_porcelain_v2_status_summary() {
        let output = b"# branch.oid deadbeef\0# branch.head feature\0# branch.upstream origin/feature\0# branch.ab +2 -3\x001 M. N... 100644 100644 100644 a b file\0? new file\0";
        assert_eq!(
            parse_porcelain_v2(output),
            Ok(GitMetadata {
                branch: "feature".into(),
                dirty_files: 2,
                ahead: 2,
                behind: 3,
                added_lines: 0,
                deleted_lines: 0,
            })
        );
    }

    #[test]
    fn parses_detached_and_rename_records_without_counting_the_source() {
        let output = b"# branch.oid deadbeef\0# branch.head (detached)\x002 R. N... 100644 100644 100644 a b R100 renamed\0original\0! ignored\0";
        assert_eq!(
            parse_porcelain_v2(output),
            Ok(GitMetadata {
                branch: "detached".into(),
                dirty_files: 1,
                ahead: 0,
                behind: 0,
                added_lines: 0,
                deleted_lines: 0,
            })
        );
    }

    #[test]
    fn parses_unborn_branch_name() {
        let output = b"# branch.oid (initial)\0# branch.head main\0";
        assert_eq!(parse_porcelain_v2(output).unwrap().branch, "main");
    }
}
