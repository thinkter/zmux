//! Bounded background metadata used by the workspace rail.

use std::env;
use std::ffi::{OsStr, OsString};
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

pub fn collect_git_metadata(repository: &Path) -> MetadataState<GitMetadata> {
    let mut child = match metadata_git_command(repository)
        .args([
            "-c",
            "core.quotepath=false",
            "status",
            "--porcelain=v1",
            "--branch",
            "--untracked-files=normal",
        ])
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
        Some(mut metadata) => {
            if let Some((added_lines, deleted_lines)) = collect_git_diff_stats(repository) {
                metadata.added_lines = added_lines;
                metadata.deleted_lines = deleted_lines;
            }
            MetadataState::Ready(metadata)
        }
        None => MetadataState::Error("git returned an invalid status summary".into()),
    }
}

fn collect_git_diff_stats(repository: &Path) -> Option<(usize, usize)> {
    let mut child = metadata_git_command(repository)
        .args([
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--numstat",
            "HEAD",
            "--",
        ])
        .spawn()
        .ok()?;
    let mut output = Vec::new();
    if child
        .stdout
        .take()?
        .take(MAX_GIT_OUTPUT_BYTES + 1)
        .read_to_end(&mut output)
        .is_err()
        || output.len() as u64 > MAX_GIT_OUTPUT_BYTES
        || !child.wait().ok()?.success()
    {
        let _ = child.kill();
        let _ = child.wait();
        return None;
    }
    Some(parse_git_numstat(&String::from_utf8_lossy(&output)))
}

/// Build a Git subprocess for automatic, terminal-discovered metadata.
///
/// Repository configuration is untrusted here. Keep every command read-only,
/// disable the repository-local fsmonitor hook, and prevent the parent process
/// from redirecting discovery or injecting configuration through `GIT_*`.
fn metadata_git_command(repository: &Path) -> Command {
    metadata_git_command_with_environment(repository, env::vars_os())
}

fn metadata_git_command_with_environment(
    repository: &Path,
    environment: impl IntoIterator<Item = (OsString, OsString)>,
) -> Command {
    // Zed's helper applies CREATE_NO_WINDOW on Windows, avoiding a console
    // flash for these periodic background probes while preserving std::process
    // inspection APIs used by the hardening tests below.
    let mut command = util::command::new_std_command("git");
    command
        .args(["--no-optional-locks", "-c", "core.fsmonitor=false"])
        .current_dir(repository)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .env_clear()
        .envs(
            environment
                .into_iter()
                .filter(|(name, _)| !is_git_environment_override(name)),
        );
    command
}

fn is_git_environment_override(name: &OsStr) -> bool {
    name.to_string_lossy()
        .get(..4)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("GIT_"))
}

fn parse_git_numstat(output: &str) -> (usize, usize) {
    output.lines().fold((0usize, 0usize), |totals, line| {
        let mut fields = line.splitn(3, '\t');
        let Some(added) = fields.next().and_then(|value| value.parse::<usize>().ok()) else {
            return totals;
        };
        let Some(deleted) = fields.next().and_then(|value| value.parse::<usize>().ok()) else {
            return totals;
        };
        (
            totals.0.saturating_add(added),
            totals.1.saturating_add(deleted),
        )
    })
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
        added_lines: 0,
        deleted_lines: 0,
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
    use std::fs;
    use std::process::{self, Output};
    use std::sync::atomic::{AtomicU64, Ordering};

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    static NEXT_TEST_REPOSITORY: AtomicU64 = AtomicU64::new(0);

    struct TestRepository {
        path: std::path::PathBuf,
    }

    impl TestRepository {
        fn new() -> Self {
            let id = NEXT_TEST_REPOSITORY.fetch_add(1, Ordering::Relaxed);
            let path =
                env::temp_dir().join(format!("zmux-git-metadata-test-{}-{id}", process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            git(&path, ["init", "--quiet"]);
            git(&path, ["config", "user.email", "zmux-tests@example.com"]);
            git(&path, ["config", "user.name", "zmux tests"]);
            git(&path, ["symbolic-ref", "HEAD", "refs/heads/main"]);
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TestRepository {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn git<I, S>(repository: &Path, args: I) -> Output
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut command = Command::new("git");
        command
            .current_dir(repository)
            .args(args)
            .env_clear()
            .envs(env::vars_os().filter(|(name, _)| !is_git_environment_override(name)));
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "test Git command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn git_config(repository: &Path, key: &str, value: &OsStr) {
        git(repository, [OsStr::new("config"), OsStr::new(key), value]);
    }

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
        assert_eq!(metadata.added_lines, 0);
        assert_eq!(metadata.deleted_lines, 0);
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

    #[test]
    fn parses_numstat_totals_and_ignores_binary_files() {
        assert_eq!(
            parse_git_numstat("8\t4\tsrc/app.rs\n7\t2\tsrc/workspaces.rs\n-\t-\timage.png\n"),
            (15, 6)
        );
    }

    #[test]
    fn collects_branch_dirty_tracking_and_line_metadata() {
        let repository = TestRepository::new();
        let tracked = repository.path().join("tracked.txt");
        fs::write(&tracked, "keep\nremove\n").unwrap();
        git(repository.path(), ["add", "tracked.txt"]);
        git(repository.path(), ["commit", "--quiet", "-m", "base"]);
        let base = git(repository.path(), ["rev-parse", "HEAD"]);
        let base = String::from_utf8(base.stdout).unwrap();
        let base = base.trim();

        git(
            repository.path(),
            ["update-ref", "refs/remotes/origin/main", base],
        );
        git(repository.path(), ["config", "remote.origin.url", "."]);
        git(
            repository.path(),
            [
                "config",
                "remote.origin.fetch",
                "+refs/heads/*:refs/remotes/origin/*",
            ],
        );
        git(
            repository.path(),
            ["config", "branch.main.remote", "origin"],
        );
        git(
            repository.path(),
            ["config", "branch.main.merge", "refs/heads/main"],
        );

        fs::write(&tracked, "keep\nremove\ncommitted\n").unwrap();
        git(repository.path(), ["add", "tracked.txt"]);
        git(
            repository.path(),
            ["commit", "--quiet", "-m", "local commit"],
        );

        git(
            repository.path(),
            ["checkout", "--quiet", "-b", "remote-side", base],
        );
        fs::write(repository.path().join("remote.txt"), "remote\n").unwrap();
        git(repository.path(), ["add", "remote.txt"]);
        git(
            repository.path(),
            ["commit", "--quiet", "-m", "remote commit"],
        );
        let remote = git(repository.path(), ["rev-parse", "HEAD"]);
        let remote = String::from_utf8(remote.stdout).unwrap();
        git(
            repository.path(),
            ["update-ref", "refs/remotes/origin/main", remote.trim()],
        );
        git(repository.path(), ["checkout", "--quiet", "main"]);

        fs::write(&tracked, "keep\ncommitted\nadded one\nadded two\n").unwrap();

        let MetadataState::Ready(metadata) = collect_git_metadata(repository.path()) else {
            panic!("expected Git metadata to be available");
        };
        assert_eq!(metadata.branch, "main");
        assert_eq!(metadata.dirty_files, 1);
        assert_eq!(metadata.ahead, 1);
        assert_eq!(metadata.behind, 1);
        assert_eq!(metadata.added_lines, 2);
        assert_eq!(metadata.deleted_lines, 1);
    }

    #[test]
    fn metadata_command_scrubs_inherited_git_overrides() {
        let repository = TestRepository::new();
        fs::write(repository.path().join("tracked.txt"), "tracked\n").unwrap();
        git(repository.path(), ["add", "tracked.txt"]);
        git(repository.path(), ["commit", "--quiet", "-m", "base"]);

        let hostile_git_dir = repository.path().join("missing-git-dir");
        let inherited = env::vars_os().chain([
            (OsString::from("GIT_DIR"), hostile_git_dir.into_os_string()),
            (
                OsString::from("GIT_INDEX_FILE"),
                OsString::from("missing-index"),
            ),
            (OsString::from("GIT_CONFIG_COUNT"), OsString::from("1")),
            (
                OsString::from("GIT_CONFIG_KEY_0"),
                OsString::from("core.fsmonitor"),
            ),
            (
                OsString::from("GIT_CONFIG_VALUE_0"),
                OsString::from("missing-helper"),
            ),
            (
                OsString::from("Git_External_Diff"),
                OsString::from("missing-helper"),
            ),
        ]);
        let mut command = metadata_git_command_with_environment(repository.path(), inherited);
        assert!(
            command
                .get_envs()
                .all(|(name, _)| !is_git_environment_override(name))
        );
        let output = command
            .args([
                "-c",
                "core.quotepath=false",
                "status",
                "--porcelain=v1",
                "--branch",
            ])
            .output()
            .unwrap();
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).starts_with("## main"));
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn automatic_metadata_never_runs_configured_git_helpers() {
        let repository = TestRepository::new();
        fs::write(
            repository.path().join(".gitattributes"),
            "*.txt diff=sentinel\n",
        )
        .unwrap();
        fs::write(repository.path().join("tracked.txt"), "base\n").unwrap();
        git(repository.path(), ["add", ".gitattributes", "tracked.txt"]);
        git(repository.path(), ["commit", "--quiet", "-m", "base"]);
        fs::write(repository.path().join("tracked.txt"), "base\nchanged\n").unwrap();

        let sentinel = repository.path().join(".git/metadata-helper-ran");
        let helper = write_sentinel_helper(repository.path());

        // First prove that each configured helper is executable in this real
        // repository. The sentinel is then cleared before hardened collection.
        git_config(repository.path(), "core.fsmonitor", helper.as_os_str());
        git(repository.path(), ["status", "--porcelain=v1"]);
        assert!(sentinel.exists(), "fsmonitor sentinel did not execute");
        fs::remove_file(&sentinel).unwrap();
        git(repository.path(), ["config", "--unset", "core.fsmonitor"]);

        git_config(repository.path(), "diff.external", helper.as_os_str());
        git(repository.path(), ["diff", "HEAD", "--"]);
        assert!(sentinel.exists(), "external diff sentinel did not execute");
        fs::remove_file(&sentinel).unwrap();
        git(repository.path(), ["config", "--unset", "diff.external"]);

        git_config(
            repository.path(),
            "diff.sentinel.textconv",
            helper.as_os_str(),
        );
        git(repository.path(), ["diff", "--textconv", "HEAD", "--"]);
        assert!(sentinel.exists(), "textconv sentinel did not execute");
        fs::remove_file(&sentinel).unwrap();

        git_config(repository.path(), "core.fsmonitor", helper.as_os_str());
        git_config(repository.path(), "diff.external", helper.as_os_str());
        let MetadataState::Ready(metadata) = collect_git_metadata(repository.path()) else {
            panic!("expected hardened Git metadata to be available");
        };
        assert_eq!(metadata.branch, "main");
        assert_eq!(metadata.dirty_files, 1);
        assert_eq!(metadata.added_lines, 1);
        assert_eq!(metadata.deleted_lines, 0);
        assert!(
            !sentinel.exists(),
            "automatic metadata executed a repository-local helper"
        );
    }

    #[cfg(unix)]
    fn write_sentinel_helper(repository: &Path) -> std::path::PathBuf {
        let helper = repository.join(".git/metadata-sentinel-helper");
        fs::write(&helper, "#!/bin/sh\n: > .git/metadata-helper-ran\nexit 0\n").unwrap();
        let mut permissions = fs::metadata(&helper).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&helper, permissions).unwrap();
        helper
    }

    #[cfg(windows)]
    fn write_sentinel_helper(repository: &Path) -> std::path::PathBuf {
        let helper = repository.join(".git/metadata-sentinel-helper.cmd");
        fs::write(
            &helper,
            "@echo off\r\ntype nul > .git\\metadata-helper-ran\r\nexit /b 0\r\n",
        )
        .unwrap();
        helper
    }
}
