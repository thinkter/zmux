//! Git metadata projected from Zed's already-maintained repository snapshots.

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
}
