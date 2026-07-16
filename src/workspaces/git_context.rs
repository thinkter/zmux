//! Git context discovery and reconciliation for logical workspaces.
//!
//! Derives each workspace's Git repositories from its live terminals' working
//! directories, keeps the project's attached worktrees in sync with the set
//! of referenced repository roots, and bridges the vendored Zed Git panel to
//! zmux's logical workspaces via [`ZmuxRepositoryScope`]. Incomplete
//! directory probes must never tear down attached state: reconciliation only
//! removes worktrees after an authoritative (fully reported) pass.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gpui::{App, Context, Entity, EntityId, Global, SharedString, Task, WeakEntity, Window};
use terminal_view::TerminalView;
use ui::prelude::*;
use workspace::Workspace;
use workspace::item::ItemHandle;

use crate::metadata::{MetadataState, collect_git_metadata};
use crate::notifications::WorkspaceId;

use super::persistence::{FailedRestoreSlot, StoredLayout};
use super::{WorkspaceEntry, WorkspacesPanel, is_shell_process, sanitize_process_label};

const ACTIVE_METADATA_INTERVAL: Duration = Duration::from_secs(5);
const INACTIVE_METADATA_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum GitDiscoveryState {
    /// A persisted layout has not finished recreating all of its terminals, so
    /// an empty or partial context cannot disprove the persisted selection.
    Restoring,
    /// Every restored terminal view is mounted, but one or more shells have
    /// not reported the working directory needed for repository discovery.
    Discovering,
    /// Every currently owned terminal contributes to repository discovery.
    Authoritative,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct WorkspaceContext {
    pub(super) working_directories: Vec<PathBuf>,
    pub(super) git_roots: Vec<PathBuf>,
    pub(super) git_root: Option<PathBuf>,
    pub(super) foreground_processes: Vec<String>,
    pub(super) shell_count: usize,
    pub(super) reported_directories: usize,
}

impl WorkspaceContext {
    /// Whether every live shell has reported a working directory. A shell's
    /// directory probe can transiently fail, and the Git roots derived from
    /// such a pass understate the workspace; they must not tear down state.
    pub(super) fn is_complete(&self) -> bool {
        self.reported_directories == self.shell_count
    }
}

#[derive(Debug, PartialEq, Eq)]
struct GitRootReconciliation {
    added: BTreeSet<PathBuf>,
    removed: BTreeSet<PathBuf>,
}

fn git_root_reference_counts<'a>(
    roots_by_workspace: impl IntoIterator<Item = &'a [PathBuf]>,
) -> BTreeMap<PathBuf, usize> {
    let mut counts = BTreeMap::new();
    for roots in roots_by_workspace {
        for root in roots {
            *counts.entry(root.clone()).or_insert(0) += 1;
        }
    }
    counts
}

fn git_root_is_referenced<'a>(
    roots_by_workspace: impl IntoIterator<Item = &'a [PathBuf]>,
    root: &Path,
) -> bool {
    roots_by_workspace
        .into_iter()
        .any(|roots| roots.iter().any(|candidate| paths_match(candidate, root)))
}

fn plan_git_root_reconciliation(
    reference_counts: BTreeMap<PathBuf, usize>,
    attached: &BTreeSet<PathBuf>,
    pending: &BTreeSet<PathBuf>,
) -> GitRootReconciliation {
    let added = reference_counts
        .keys()
        .filter(|root| !attached.contains(*root) && !pending.contains(*root))
        .cloned()
        .collect();
    let removed = attached
        .iter()
        .filter(|root| !reference_counts.contains_key(*root))
        .cloned()
        .collect();
    GitRootReconciliation { added, removed }
}

fn reconcile_selected_git_root(
    selected: &mut Option<PathBuf>,
    discovered_roots: &[PathBuf],
    discovery: GitDiscoveryState,
) {
    if discovery != GitDiscoveryState::Authoritative {
        return;
    }
    if selected.as_ref().is_none_or(|root| {
        !discovered_roots
            .iter()
            .any(|discovered| paths_match(root, discovered))
    }) {
        *selected = discovered_roots.first().cloned();
    }
}

fn git_contexts_are_authoritative<'a>(
    entries: impl IntoIterator<Item = &'a WorkspaceEntry>,
) -> bool {
    entries.into_iter().all(|entry| entry.context_authoritative)
}

fn retain_completed_worktree_scan(root_is_referenced: bool, contexts_authoritative: bool) -> bool {
    root_is_referenced || !contexts_authoritative
}

fn track_pending_worktree(pending: &mut BTreeSet<PathBuf>, root: PathBuf) -> bool {
    pending.insert(root)
}

/// Bridges the vendored Zed Git panel to zmux's logical workspaces.
#[derive(Default)]
pub struct ZmuxRepositoryScope {
    panels: Mutex<HashMap<EntityId, WeakEntity<WorkspacesPanel>>>,
}

struct ZmuxRepositoryScopeGlobal(Arc<ZmuxRepositoryScope>);
impl Global for ZmuxRepositoryScopeGlobal {}

pub fn install_git_repository_scope(cx: &mut App) {
    let scope = Arc::new(ZmuxRepositoryScope::default());
    git_ui::set_repository_scope(scope.clone(), cx);
    cx.set_global(ZmuxRepositoryScopeGlobal(scope));
}

pub fn register_git_repository_scope(
    project: &Entity<project::Project>,
    panel: &Entity<WorkspacesPanel>,
    cx: &App,
) {
    cx.global::<ZmuxRepositoryScopeGlobal>()
        .0
        .register(project, panel);
}

impl ZmuxRepositoryScope {
    pub fn register(&self, project: &Entity<project::Project>, panel: &Entity<WorkspacesPanel>) {
        self.panels
            .lock()
            .expect("repository scope registry poisoned")
            .insert(project.entity_id(), panel.downgrade());
    }

    fn panel_for(&self, project: &Entity<project::Project>) -> Option<WeakEntity<WorkspacesPanel>> {
        self.panels
            .lock()
            .expect("repository scope registry poisoned")
            .get(&project.entity_id())
            .cloned()
    }
}

impl git_ui::RepositoryScope for ZmuxRepositoryScope {
    fn repositories(
        &self,
        project: &Entity<project::Project>,
        cx: &App,
    ) -> Vec<Entity<project::git_store::Repository>> {
        let roots = self
            .panel_for(project)
            .and_then(|panel| panel.upgrade())
            .map(|panel| panel.read(cx).active_git_roots().to_vec())
            .unwrap_or_default();
        project
            .read(cx)
            .git_store()
            .read(cx)
            .repositories()
            .values()
            .filter(|repo| {
                roots.iter().any(|root| {
                    paths_match(
                        repo.read(cx).snapshot().work_directory_abs_path.as_ref(),
                        root,
                    )
                })
            })
            .cloned()
            .collect()
    }

    fn display_name(
        &self,
        repository: &Entity<project::git_store::Repository>,
        cx: &App,
    ) -> SharedString {
        repository
            .read(cx)
            .snapshot()
            .work_directory_abs_path
            .display()
            .to_string()
            .into()
    }

    fn select(
        &self,
        project: &Entity<project::Project>,
        repository: Entity<project::git_store::Repository>,
        cx: &mut App,
    ) {
        let root = repository
            .read(cx)
            .snapshot()
            .work_directory_abs_path
            .to_path_buf();
        if let Some(panel) = self.panel_for(project).and_then(|panel| panel.upgrade()) {
            panel.update(cx, |panel, cx| panel.select_git_root(root.clone(), cx));
        }
        repository.update(cx, |repository, cx| repository.set_as_active_repository(cx));
    }

    fn active_worktree_paths(&self, project: &Entity<project::Project>, cx: &App) -> Vec<PathBuf> {
        self.panel_for(project)
            .and_then(|panel| panel.upgrade())
            .and_then(|panel| panel.read(cx).active_git_root())
            .into_iter()
            .collect()
    }

    fn open_worktree_paths(&self, project: &Entity<project::Project>, cx: &App) -> Vec<PathBuf> {
        self.panel_for(project)
            .and_then(|panel| panel.upgrade())
            .map(|panel| panel.read(cx).open_git_roots())
            .unwrap_or_default()
    }

    fn close_open_worktree(
        &self,
        project: &Entity<project::Project>,
        path: &Path,
        window: &mut Window,
        cx: &mut App,
    ) -> bool {
        self.panel_for(project)
            .and_then(|panel| panel.upgrade())
            .is_some_and(|panel| {
                panel.update(cx, |panel, cx| {
                    panel.close_workspace_for_git_root(path, window, cx)
                })
            })
    }
}

impl WorkspacesPanel {
    fn active_git_roots(&self) -> &[PathBuf] {
        self.entries
            .iter()
            .find(|entry| entry.id == self.active)
            .map(|entry| entry.context.git_roots.as_slice())
            .unwrap_or_default()
    }

    pub(super) fn active_git_root(&self) -> Option<PathBuf> {
        self.entries
            .iter()
            .find(|entry| entry.id == self.active)
            .and_then(|entry| {
                entry
                    .selected_git_root
                    .clone()
                    .or_else(|| entry.context.git_root.clone())
                    .or_else(|| {
                        entry
                            .default_directory
                            .as_deref()
                            .and_then(nearest_git_root)
                    })
            })
    }

    fn open_git_roots(&self) -> Vec<PathBuf> {
        let mut roots = BTreeSet::new();
        for entry in &self.entries {
            roots.extend(entry.context.git_roots.iter().cloned());
            roots.extend(entry.selected_git_root.iter().cloned());
            roots.extend(entry.worktree_paths.iter().cloned());
            roots.extend(
                entry
                    .default_directory
                    .as_deref()
                    .and_then(nearest_git_root),
            );
        }
        roots.into_iter().collect()
    }

    fn select_git_root(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| entry.id == self.active)
            && entry
                .context
                .git_roots
                .iter()
                .any(|candidate| paths_match(candidate, &root))
        {
            entry.selected_git_root = Some(root);
            self.request_metadata_refreshes(cx);
            self.persist_session(cx);
            cx.notify();
        }
    }

    pub(super) fn workspace_id_for_git_root(&self, path: &Path) -> Option<WorkspaceId> {
        self.entries.iter().find_map(|entry| {
            (entry
                .selected_git_root
                .as_deref()
                .is_some_and(|root| paths_match(root, path))
                || entry
                    .default_directory
                    .as_deref()
                    .is_some_and(|root| paths_match(root, path))
                || entry
                    .worktree_paths
                    .iter()
                    .any(|root| paths_match(root, path))
                || entry
                    .context
                    .git_roots
                    .iter()
                    .any(|root| paths_match(root, path)))
            .then_some(entry.id)
        })
    }

    fn close_workspace_for_git_root(
        &mut self,
        path: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(id) = self.workspace_id_for_git_root(path) else {
            return false;
        };
        if id == self.active || self.entries.len() <= 1 {
            return false;
        }
        self.close_workspace(id, window, cx);
        true
    }

    pub(super) fn promote_restored_git_discovery(&mut self) -> bool {
        let mut changed = false;
        for entry in &mut self.entries {
            if entry.git_discovery != GitDiscoveryState::Discovering {
                continue;
            }
            // The live shell directories are authoritative once every mounted
            // terminal has reported one. They need not match the snapshot: a
            // shell rc file may intentionally cd elsewhere, or a repository
            // may have been deleted since the previous launch.
            if entry.context_authoritative {
                entry.restore = None;
                entry.git_discovery = GitDiscoveryState::Authoritative;
                changed = true;
            }
        }
        changed
    }

    pub(super) fn reconcile_git_context(&mut self, cx: &mut Context<Self>) {
        for entry in &mut self.entries {
            // A timed-out incomplete context is authoritative enough to stop
            // blocking persistence and cleanup, but never authoritative enough
            // to erase a user's selected repository. Reconcile selection only
            // from a fully observed shell set.
            if !entry.context.is_complete() {
                continue;
            }
            reconcile_selected_git_root(
                &mut entry.selected_git_root,
                &entry.context.git_roots,
                entry.git_discovery,
            );
        }

        let reference_counts = git_root_reference_counts(
            self.entries
                .iter()
                .map(|entry| entry.context.git_roots.as_slice()),
        );
        let attached = self.attached_worktrees.keys().cloned().collect();
        let reconciliation =
            plan_git_root_reconciliation(reference_counts, &attached, &self.pending_worktrees);
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let project = workspace.read(cx).project().clone();

        // A pass where any shell's directory probe failed understates the
        // referenced roots; removing worktrees from it would detach and
        // rescan repositories that are still in use. Wait for a stable pass.
        let contexts_authoritative = git_contexts_are_authoritative(&self.entries);
        if contexts_authoritative {
            for root in reconciliation.removed {
                if let Some(worktree) = self.attached_worktrees.remove(&root) {
                    let id = worktree.read(cx).id();
                    project.update(cx, |project, cx| project.remove_worktree(id, cx));
                }
            }
        }

        for root in reconciliation.added {
            // Keep the mutation outside the debug assertion: assertions are
            // compiled out of release builds, but pending scans are required
            // there to prevent duplicate worktree creation every refresh.
            let inserted = track_pending_worktree(&mut self.pending_worktrees, root.clone());
            debug_assert!(
                inserted,
                "planned additions are disjoint from pending scans"
            );
            // `find_or_create_worktree` accepts any existing ancestor worktree,
            // and Zed deliberately disables Git discovery for invisible
            // worktrees. Both behaviors are wrong for terminal-driven discovery:
            // attach an exact, Git-tracked worktree root. zmux has no project
            // panel, so making it visible affects only Zed's internal scanning.
            let existing = project
                .read(cx)
                .worktrees(cx)
                .find(|worktree| paths_match(worktree.read(cx).abs_path().as_ref(), &root));
            let task = if let Some(worktree) = existing {
                Task::ready(Ok(worktree))
            } else {
                project.update(cx, |project, cx| project.create_worktree(&root, true, cx))
            };
            let project = project.clone();
            cx.spawn(async move |this, cx| {
                let result = task.await;
                this.update(cx, |this, cx| {
                    this.pending_worktrees.remove(&root);
                    match result {
                        Ok(worktree)
                            if retain_completed_worktree_scan(
                                git_root_is_referenced(
                                    this.entries
                                        .iter()
                                        .map(|entry| entry.context.git_roots.as_slice()),
                                    &root,
                                ),
                                git_contexts_are_authoritative(&this.entries),
                            ) =>
                        {
                            this.attached_worktrees.insert(root.clone(), worktree);
                            this.activate_selected_repository(cx);
                        }
                        Ok(worktree) => {
                            // Repository discovery moved on while this worktree
                            // was scanning. Remove the Project's ownership as
                            // well as dropping this late result.
                            let id = worktree.read(cx).id();
                            project.update(cx, |project, cx| project.remove_worktree(id, cx));
                        }
                        Err(error) => {
                            eprintln!(
                                "failed to attach Git repository {}: {error:#}",
                                root.display()
                            );
                        }
                    }
                })
                .ok();
            })
            .detach();
        }
        self.activate_selected_repository(cx);
    }

    pub(crate) fn finish_restored_git_discovery(
        &mut self,
        id: WorkspaceId,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = self.entries.iter_mut().find(|entry| entry.id == id) else {
            return;
        };
        if entry.git_discovery == GitDiscoveryState::Restoring {
            entry.git_discovery = GitDiscoveryState::Discovering;
        }
        entry.failed_restores.clear();
        self.refresh_workspace_contexts(cx);
        self.request_metadata_refreshes(cx);
        self.persist_session(cx);
    }

    pub(crate) fn finish_restored_git_discovery_with_failures(
        &mut self,
        id: WorkspaceId,
        failed: Vec<FailedRestoreSlot>,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = self.entries.iter_mut().find(|entry| entry.id == id) else {
            return;
        };
        entry.failed_restores = failed;
        // The live layout can now persist immediately. Failed tabs are overlaid
        // into that snapshot at their original pane path/index and will retry
        // on the next launch; the complete original snapshot is no longer the
        // only persistence source.
        entry.restore = None;
        entry.git_discovery = GitDiscoveryState::Authoritative;
        self.refresh_workspace_contexts(cx);
        self.request_metadata_refreshes(cx);
        self.persist_session(cx);
    }

    fn activate_selected_repository(&self, cx: &mut Context<Self>) {
        let Some(root) = self
            .entries
            .iter()
            .find(|entry| entry.id == self.active)
            .and_then(|entry| entry.selected_git_root.as_ref())
        else {
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let git_store = workspace.read(cx).project().read(cx).git_store().clone();
        let repository = git_store
            .read(cx)
            .repositories()
            .values()
            .find(|repo| {
                paths_match(
                    repo.read(cx).snapshot().work_directory_abs_path.as_ref(),
                    root,
                )
            })
            .cloned();
        if let Some(repository) = repository {
            repository.update(cx, |repository, cx| repository.set_as_active_repository(cx));
        }
    }

    pub(super) fn request_metadata_refreshes(&mut self, cx: &mut Context<Self>) {
        let now = Instant::now();
        let mut requests = Vec::new();
        for entry in &mut self.entries {
            let root = entry.selected_git_root.clone();
            if entry.metadata_root != root {
                entry.metadata_root = root.clone();
                entry.metadata_refreshed_at = None;
                entry.git = MetadataState::NotRequested;
            }
            let Some(root) = root else {
                continue;
            };
            let interval = if entry.id == self.active {
                ACTIVE_METADATA_INTERVAL
            } else {
                INACTIVE_METADATA_INTERVAL
            };
            let is_due = entry
                .metadata_refreshed_at
                .is_none_or(|refreshed| now.duration_since(refreshed) >= interval);
            if is_due && !matches!(entry.git, MetadataState::Pending) {
                entry.git = MetadataState::Pending;
                entry.metadata_refreshed_at = Some(now);
                requests.push((entry.id, root));
            }
        }

        for (id, root) in requests {
            let requested_root = root.clone();
            let collection = cx.background_spawn(async move { collect_git_metadata(&root) });
            cx.spawn(async move |this, cx| {
                let git = collection.await;
                this.update(cx, |this, cx| {
                    if let Some(entry) = this.entries.iter_mut().find(|entry| entry.id == id)
                        && entry.metadata_root.as_ref() == Some(&requested_root)
                    {
                        entry.git = git;
                        cx.notify();
                    }
                })
                .ok();
            })
            .detach();
        }
    }
}

pub(super) fn workspace_context_for_active_workspace(
    workspace: &Workspace,
    cx: &App,
) -> WorkspaceContext {
    let mut context = WorkspaceContext::default();
    for pane in workspace.panes() {
        for item in pane.read(cx).items() {
            add_item_to_workspace_context(item.as_ref(), &mut context, cx);
        }
    }
    finalize_workspace_context(context)
}

pub(super) fn workspace_context_for_stored_layout(
    layout: &StoredLayout,
    cx: &App,
) -> WorkspaceContext {
    fn visit(layout: &StoredLayout, context: &mut WorkspaceContext, cx: &App) {
        match layout {
            StoredLayout::Leaf { items, .. } => {
                for item in items {
                    add_item_to_workspace_context(item.as_ref(), context, cx);
                }
            }
            StoredLayout::Split { first, second, .. } => {
                visit(first, context, cx);
                visit(second, context, cx);
            }
        }
    }

    let mut context = WorkspaceContext::default();
    visit(layout, &mut context, cx);
    finalize_workspace_context(context)
}

fn add_item_to_workspace_context(item: &dyn ItemHandle, context: &mut WorkspaceContext, cx: &App) {
    let Some(terminal_view) = item.act_as::<TerminalView>(cx) else {
        return;
    };
    let terminal = terminal_view.read(cx).terminal().clone();
    let terminal = terminal.read(cx);
    context.shell_count += 1;
    if let Some(directory) = terminal.working_directory() {
        context.reported_directories += 1;
        context.working_directories.push(directory);
    }
    if let Some(process) = terminal.foreground_process_command_name()
        && !is_shell_process(&process)
        && let Some(process) = sanitize_process_label(&process)
    {
        context.foreground_processes.push(process);
    }
}

fn finalize_workspace_context(mut context: WorkspaceContext) -> WorkspaceContext {
    context.working_directories.sort();
    context.working_directories.dedup();

    let processes = context
        .foreground_processes
        .drain(..)
        .collect::<BTreeSet<_>>();
    context.foreground_processes = processes.into_iter().take(8).collect();

    context.git_roots = context
        .working_directories
        .iter()
        .filter_map(|directory| nearest_git_root(directory))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    if context.git_roots.len() == 1 {
        context.git_root = context.git_roots.first().cloned();
    }
    context
}

fn nearest_git_root(directory: &Path) -> Option<PathBuf> {
    directory
        .ancestors()
        .find(|ancestor| ancestor.join(".git").exists())
        .map(Path::to_path_buf)
}

/// Compare existing filesystem paths without replacing their user-visible form.
///
/// `canonicalize` rewrites `/var` to `/private/var` on macOS and may introduce
/// verbatim (`\\?\`) or long-name paths on Windows. Those are useful identity
/// keys, but persisting them changes workspace labels and breaks equality with
/// shell-reported paths, so canonicalization is confined to comparison.
fn paths_match(left: &Path, right: &Path) -> bool {
    left == right
        || std::fs::canonicalize(left)
            .ok()
            .zip(std::fs::canonicalize(right).ok())
            .is_some_and(|(left, right)| left == right)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{LayoutNodeSnapshot, LayoutSnapshot, TerminalSnapshot};

    #[cfg(unix)]
    #[test]
    fn path_identity_matches_aliases_without_rewriting_logical_paths() {
        let base =
            std::env::temp_dir().join(format!("zmux-path-identity-{}", uuid::Uuid::new_v4()));
        let physical = base.join("physical");
        let logical = base.join("logical");
        std::fs::create_dir_all(&physical).unwrap();
        std::os::unix::fs::symlink(&physical, &logical).unwrap();

        assert!(paths_match(&logical, &physical));
        assert_ne!(logical, physical);

        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn git_context_keeps_independent_roots_and_ignores_non_repo_terminals() {
        let base = std::env::temp_dir().join(format!("zmux-git-context-{}", uuid::Uuid::new_v4()));
        let repo_a = base.join("a");
        let repo_b = base.join("b");
        let outside = base.join("outside");
        std::fs::create_dir_all(repo_a.join(".git")).unwrap();
        std::fs::create_dir_all(repo_a.join("src")).unwrap();
        std::fs::create_dir_all(repo_b.join(".git")).unwrap();
        std::fs::create_dir_all(outside.clone()).unwrap();

        let context = finalize_workspace_context(WorkspaceContext {
            working_directories: vec![repo_a.join("src"), outside, repo_b.clone()],
            shell_count: 3,
            ..WorkspaceContext::default()
        });

        assert_eq!(context.git_roots, vec![repo_a, repo_b]);
        assert_eq!(context.git_root, None);
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn git_context_collapses_multiple_terminals_in_one_repository() {
        let base =
            std::env::temp_dir().join(format!("zmux-single-git-context-{}", std::process::id()));
        std::fs::create_dir_all(base.join(".git")).unwrap();
        std::fs::create_dir_all(base.join("api")).unwrap();
        std::fs::create_dir_all(base.join("web")).unwrap();

        let context = finalize_workspace_context(WorkspaceContext {
            working_directories: vec![base.join("api"), base.join("web")],
            shell_count: 2,
            ..WorkspaceContext::default()
        });

        assert_eq!(context.git_roots, vec![base.clone()]);
        assert_eq!(context.git_root, Some(base.clone()));
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn inactive_restored_workspace_keeps_selection_before_discovery() {
        let selected_root = PathBuf::from("/repos/selected");
        let mut selected = Some(selected_root.clone());

        reconcile_selected_git_root(&mut selected, &[], GitDiscoveryState::Restoring);

        assert_eq!(selected, Some(selected_root));
    }

    #[test]
    fn active_restore_waits_for_complete_multi_repo_discovery() {
        let first_root = PathBuf::from("/repos/first");
        let selected_root = PathBuf::from("/repos/selected");
        let mut selected = Some(selected_root.clone());

        // The first restored terminal must not win merely because its shell
        // was created before the selected repository's terminal.
        reconcile_selected_git_root(
            &mut selected,
            std::slice::from_ref(&first_root),
            GitDiscoveryState::Restoring,
        );
        assert_eq!(selected, Some(selected_root.clone()));
        reconcile_selected_git_root(
            &mut selected,
            std::slice::from_ref(&first_root),
            GitDiscoveryState::Discovering,
        );
        assert_eq!(selected, Some(selected_root.clone()));

        reconcile_selected_git_root(
            &mut selected,
            &[first_root, selected_root.clone()],
            GitDiscoveryState::Authoritative,
        );
        assert_eq!(selected, Some(selected_root));
    }

    #[test]
    fn authoritative_discovery_replaces_a_missing_selection() {
        let discovered_root = PathBuf::from("/repos/current");
        let mut selected = Some(PathBuf::from("/repos/gone"));

        reconcile_selected_git_root(
            &mut selected,
            std::slice::from_ref(&discovered_root),
            GitDiscoveryState::Authoritative,
        );

        assert_eq!(selected, Some(discovered_root));
    }

    #[test]
    fn complete_live_context_is_authoritative_even_when_snapshot_root_moved() {
        let context = WorkspaceContext {
            working_directories: vec![PathBuf::from("/repos/current")],
            git_roots: vec![PathBuf::from("/repos/current")],
            shell_count: 1,
            reported_directories: 1,
            ..WorkspaceContext::default()
        };
        assert!(context.is_complete());

        let incomplete = WorkspaceContext {
            shell_count: 2,
            reported_directories: 1,
            ..context
        };
        assert!(!incomplete.is_complete());
    }

    #[test]
    fn incomplete_context_retains_a_completed_worktree_scan_until_a_stable_pass() {
        assert!(retain_completed_worktree_scan(false, false));
        assert!(!retain_completed_worktree_scan(false, true));
        assert!(retain_completed_worktree_scan(true, true));
    }

    #[test]
    fn pending_scan_tracking_mutates_in_release_profiles() {
        let root = PathBuf::from("/repos/pending");
        let mut pending = BTreeSet::new();

        assert!(track_pending_worktree(&mut pending, root.clone()));
        assert!(pending.contains(&root));
        assert!(!track_pending_worktree(&mut pending, root));
    }

    #[test]
    fn git_root_reconciliation_counts_shared_workspace_ownership() {
        let shared = PathBuf::from("/repos/shared");
        let first = PathBuf::from("/repos/first");
        let second = PathBuf::from("/repos/second");
        let stale = PathBuf::from("/repos/stale");
        let workspace_a = [shared.clone(), first.clone()];
        let workspace_b = [shared.clone(), second.clone()];
        let counts = git_root_reference_counts([workspace_a.as_slice(), workspace_b.as_slice()]);
        assert_eq!(counts[&shared], 2);
        assert_eq!(counts[&first], 1);
        assert_eq!(counts[&second], 1);
        let attached = BTreeSet::from([shared.clone(), stale.clone()]);
        let pending = BTreeSet::from([second.clone()]);

        let reconciliation = plan_git_root_reconciliation(counts, &attached, &pending);

        assert_eq!(reconciliation.added, BTreeSet::from([first]));
        assert_eq!(reconciliation.removed, BTreeSet::from([stale]));
    }

    #[test]
    fn late_attachment_is_unreferenced_after_its_workspace_moves_on() {
        let late = PathBuf::from("/repos/late");
        let current = PathBuf::from("/repos/current");
        let roots = [current];

        assert!(!git_root_is_referenced([roots.as_slice()], &late));
    }

    #[test]
    fn git_root_reconciliation_stress_returns_to_active_roots() {
        let visited = (0..256)
            .map(|index| PathBuf::from(format!("/repos/visited-{index}")))
            .collect::<BTreeSet<_>>();
        let active = visited.iter().rev().take(3).cloned().collect::<Vec<_>>();
        let reference_counts = git_root_reference_counts([active.as_slice()]);

        let reconciliation =
            plan_git_root_reconciliation(reference_counts, &visited, &BTreeSet::new());
        let mut retained = visited;
        for root in reconciliation.removed {
            retained.remove(&root);
        }
        retained.extend(reconciliation.added);

        assert_eq!(retained, active.into_iter().collect());
        assert_eq!(retained.len(), 3);
    }

    #[gpui::test]
    async fn panel_restores_exact_selected_root_after_multi_repo_discovery(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.executor().allow_parking();
        let base =
            std::env::temp_dir().join(format!("zmux-multi-repo-restore-{}", uuid::Uuid::new_v4()));
        let first = base.join("first");
        let selected = base.join("selected");
        std::fs::create_dir_all(first.join(".git")).unwrap();
        std::fs::create_dir_all(selected.join(".git")).unwrap();

        let open = cx.update(|cx| {
            crate::app::init_zmux(cx);
            crate::app::open_zmux_workspace_at(None, base.clone(), cx)
        });
        let opened = open.await.expect("workspace should open");
        let panel = opened.workspace.read_with(cx, |workspace, cx| {
            workspace
                .panel::<WorkspacesPanel>(cx)
                .expect("workspaces panel should be installed")
        });
        panel.update(cx, |panel, cx| {
            panel._context_refresh_task = Task::ready(());
            panel._agent_refresh_task = Task::ready(());
            panel.entries.push(WorkspaceEntry {
                id: 2,
                manual_name: None,
                worktree_name: None,
                worktree_paths: Vec::new(),
                automatic_name: "Restored".into(),
                context: WorkspaceContext::default(),
                context_authoritative: false,
                incomplete_context_refreshes: 0,
                default_directory: None,
                selected_git_root: Some(selected.clone()),
                git_discovery: GitDiscoveryState::Restoring,
                git: MetadataState::NotRequested,
                metadata_root: None,
                metadata_refreshed_at: None,
                restore: Some(LayoutSnapshot {
                    root: LayoutNodeSnapshot::Leaf {
                        tabs: vec![
                            TerminalSnapshot::fresh_shell(Some(first.clone())),
                            TerminalSnapshot::fresh_shell(Some(selected.clone())),
                        ],
                        active_tab: 1,
                        focused: true,
                    },
                }),
                failed_restores: Vec::new(),
                stored: None,
            });
            panel.next_id = 3;
            // This is the periodic pass that used to erase an inactive
            // restored workspace's selection before it was ever activated.
            panel.reconcile_git_context(cx);
        });
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.entries[1].selected_git_root.clone()),
            Some(selected.clone())
        );

        opened
            .window
            .update(cx, |_, window, cx| {
                panel.update(cx, |panel, cx| panel.activate_workspace(2, window, cx));
            })
            .expect("window should remain open");
        assert_eq!(
            panel.read_with(cx, |panel, _| {
                let entry = panel.entries.iter().find(|entry| entry.id == 2).unwrap();
                (entry.git_discovery, entry.selected_git_root.clone())
            }),
            (GitDiscoveryState::Restoring, Some(selected.clone()))
        );

        for _ in 0..200 {
            cx.run_until_parked();
            panel.update(cx, |panel, cx| panel.refresh_workspace_contexts(cx));
            let restored = panel.read_with(cx, |panel, _| {
                let entry = panel.entries.iter().find(|entry| entry.id == 2).unwrap();
                entry.git_discovery == GitDiscoveryState::Authoritative
                    && entry.context.git_roots == vec![first.clone(), selected.clone()]
            });
            if restored {
                break;
            }
            cx.background_executor
                .timer(Duration::from_millis(10))
                .await;
        }
        let live_directories = opened.workspace.read_with(cx, |workspace, cx| {
            workspace_context_for_active_workspace(workspace, cx).working_directories
        });
        panel.read_with(cx, |panel, _| {
            let entry = panel.entries.iter().find(|entry| entry.id == 2).unwrap();
            assert_eq!(entry.git_discovery, GitDiscoveryState::Authoritative);
            assert_eq!(
                entry.context.git_roots,
                vec![first.clone(), selected.clone()],
                "shell_count={}, working_directories={:?}, live_directories={live_directories:?}",
                entry.context.shell_count,
                entry.context.working_directories,
            );
            assert_eq!(entry.selected_git_root, Some(selected.clone()));
        });

        let _ = std::fs::remove_dir_all(base);
    }

    #[gpui::test]
    async fn empty_restored_snapshot_becomes_authoritative_on_activation(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.executor().allow_parking();
        let base =
            std::env::temp_dir().join(format!("zmux-empty-restore-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&base).unwrap();
        let stale_selection = base.join("stale");

        let open = cx.update(|cx| {
            crate::app::init_zmux(cx);
            crate::app::open_zmux_workspace_at(None, base.clone(), cx)
        });
        let opened = open.await.expect("workspace should open");
        let panel = opened.workspace.read_with(cx, |workspace, cx| {
            workspace.panel::<WorkspacesPanel>(cx).unwrap()
        });
        panel.update(cx, |panel, _| {
            panel._context_refresh_task = Task::ready(());
            panel._agent_refresh_task = Task::ready(());
            panel.entries.push(WorkspaceEntry {
                id: 2,
                manual_name: None,
                worktree_name: None,
                worktree_paths: Vec::new(),
                automatic_name: "Empty restore".into(),
                context: WorkspaceContext::default(),
                context_authoritative: false,
                incomplete_context_refreshes: 0,
                default_directory: None,
                selected_git_root: Some(stale_selection),
                git_discovery: GitDiscoveryState::Restoring,
                git: MetadataState::NotRequested,
                metadata_root: None,
                metadata_refreshed_at: None,
                restore: Some(LayoutSnapshot {
                    root: LayoutNodeSnapshot::Leaf {
                        tabs: Vec::new(),
                        active_tab: 0,
                        focused: true,
                    },
                }),
                failed_restores: Vec::new(),
                stored: None,
            });
            panel.next_id = 3;
        });

        opened
            .window
            .update(cx, |_, window, cx| {
                panel.update(cx, |panel, cx| panel.activate_workspace(2, window, cx));
            })
            .expect("window should remain open");

        panel.read_with(cx, |panel, _| {
            let entry = panel.entries.iter().find(|entry| entry.id == 2).unwrap();
            assert_eq!(entry.git_discovery, GitDiscoveryState::Authoritative);
            assert_eq!(entry.selected_git_root, None);
        });
        let _ = std::fs::remove_dir_all(base);
    }

    #[gpui::test]
    async fn interrupted_restore_retries_full_snapshot_before_becoming_authoritative(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.executor().allow_parking();
        let base =
            std::env::temp_dir().join(format!("zmux-interrupted-restore-{}", uuid::Uuid::new_v4()));
        let selected = base.join("selected");
        std::fs::create_dir_all(selected.join(".git")).unwrap();

        let open = cx.update(|cx| {
            crate::app::init_zmux(cx);
            crate::app::open_zmux_workspace_at(None, base.clone(), cx)
        });
        let opened = open.await.expect("workspace should open");
        let panel = opened.workspace.read_with(cx, |workspace, cx| {
            workspace.panel::<WorkspacesPanel>(cx).unwrap()
        });
        panel.update(cx, |panel, _| {
            panel._context_refresh_task = Task::ready(());
            panel._agent_refresh_task = Task::ready(());
            panel.entries.push(WorkspaceEntry {
                id: 2,
                manual_name: None,
                worktree_name: None,
                worktree_paths: Vec::new(),
                automatic_name: "Interrupted restore".into(),
                context: WorkspaceContext::default(),
                context_authoritative: false,
                incomplete_context_refreshes: 0,
                default_directory: Some(selected.clone()),
                selected_git_root: Some(selected.clone()),
                git_discovery: GitDiscoveryState::Restoring,
                git: MetadataState::NotRequested,
                metadata_root: None,
                metadata_refreshed_at: None,
                restore: Some(LayoutSnapshot {
                    root: LayoutNodeSnapshot::Leaf {
                        tabs: vec![TerminalSnapshot::fresh_shell(Some(selected.clone()))],
                        active_tab: 0,
                        focused: true,
                    },
                }),
                failed_restores: Vec::new(),
                stored: None,
            });
            panel.next_id = 3;
        });

        // Switch away in the same foreground turn, before the restored shell
        // can attach. The parked empty layout must not replace the full snapshot.
        opened
            .window
            .update(cx, |_, window, cx| {
                panel.update(cx, |panel, cx| {
                    panel.activate_workspace(2, window, cx);
                    panel.activate_workspace(1, window, cx);
                });
            })
            .expect("window should remain open");
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            let entry = panel.entries.iter().find(|entry| entry.id == 2).unwrap();
            assert_eq!(entry.git_discovery, GitDiscoveryState::Restoring);
            assert_eq!(entry.selected_git_root, Some(selected.clone()));
        });

        opened
            .window
            .update(cx, |_, window, cx| {
                panel.update(cx, |panel, cx| panel.activate_workspace(2, window, cx));
            })
            .expect("window should remain open");
        // The terminal process-info cache is deliberately refreshed on a
        // short delay. Wait for the restored cwd, not merely the bounded
        // incomplete-context fallback becoming authoritative.
        for _ in 0..200 {
            cx.run_until_parked();
            panel.update(cx, |panel, cx| panel.refresh_workspace_contexts(cx));
            if panel.read_with(cx, |panel, _| {
                panel
                    .entries
                    .iter()
                    .find(|entry| entry.id == 2)
                    .is_some_and(|entry| {
                        entry.git_discovery == GitDiscoveryState::Authoritative
                            && entry.context.git_roots == vec![selected.clone()]
                    })
            }) {
                break;
            }
            cx.background_executor
                .timer(Duration::from_millis(10))
                .await;
        }
        panel.read_with(cx, |panel, _| {
            let entry = panel.entries.iter().find(|entry| entry.id == 2).unwrap();
            assert_eq!(entry.git_discovery, GitDiscoveryState::Authoritative);
            assert_eq!(entry.selected_git_root, Some(selected.clone()));
            assert_eq!(entry.context.git_roots, vec![selected.clone()]);
        });

        let _ = std::fs::remove_dir_all(base);
    }

    #[gpui::test]
    async fn worktree_paths_open_once_and_keep_their_logical_identity(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.executor().allow_parking();
        let base =
            std::env::temp_dir().join(format!("zmux-logical-worktree-{}", uuid::Uuid::new_v4()));
        let first = base.join("feature").join("repo");
        let second = base.join("feature").join("docs");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();

        let open = cx.update(|cx| {
            crate::app::init_zmux(cx);
            crate::app::open_zmux_workspace_at(None, base.clone(), cx)
        });
        let opened = open.await.expect("workspace should open");
        let panel = opened.workspace.read_with(cx, |workspace, cx| {
            workspace.panel::<WorkspacesPanel>(cx).unwrap()
        });
        panel.update(cx, |panel, _| {
            panel._context_refresh_task = Task::ready(());
            panel._agent_refresh_task = Task::ready(());
        });

        opened
            .window
            .update(cx, |_, window, cx| {
                panel.update(cx, |panel, cx| {
                    panel.open_created_worktrees(
                        vec![second.clone(), first.clone(), first.clone()],
                        "feature".into(),
                        window,
                        cx,
                    );
                });
            })
            .unwrap();

        let worktree_id = panel.read_with(cx, |panel, _| {
            assert_eq!(panel.entries.len(), 2);
            let entry = panel
                .entries
                .iter()
                .find(|entry| entry.id == panel.active)
                .unwrap();
            assert_eq!(entry.worktree_name.as_deref(), Some("feature"));
            assert_eq!(entry.display_name(), "feature");
            assert_eq!(entry.worktree_paths, vec![second.clone(), first.clone()]);
            entry.id
        });

        opened
            .window
            .update(cx, |_, window, cx| {
                panel.update(cx, |panel, cx| {
                    panel.activate_workspace(1, window, cx);
                    panel.open_worktree(first.clone(), "feature".into(), window, cx);
                });
            })
            .unwrap();

        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.entries.len(), 2, "an open worktree was duplicated");
            assert_eq!(panel.active, worktree_id);
            assert_eq!(panel.open_git_roots(), vec![second.clone(), first.clone()]);
        });

        let _ = std::fs::remove_dir_all(base);
    }

    #[gpui::test]
    async fn closing_workspace_removes_only_its_project_worktree(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let base =
            std::env::temp_dir().join(format!("zmux-worktree-ownership-{}", uuid::Uuid::new_v4()));
        let shared = base.join("shared");
        let unique = base.join("unique");
        std::fs::create_dir_all(shared.join(".git")).unwrap();
        std::fs::create_dir_all(unique.join(".git")).unwrap();

        let open = cx.update(|cx| {
            crate::app::init_zmux(cx);
            crate::app::open_zmux_workspace_at(None, base.clone(), cx)
        });
        let opened = open.await.expect("workspace should open");
        let panel = opened.workspace.read_with(cx, |workspace, cx| {
            workspace
                .panel::<WorkspacesPanel>(cx)
                .expect("workspaces panel should be installed")
        });
        panel.update(cx, |panel, cx| {
            // Keep the test's synthetic terminal contexts stable instead of
            // letting the periodic live-terminal refresh replace them.
            panel._context_refresh_task = Task::ready(());
            panel._agent_refresh_task = Task::ready(());
            let active = panel
                .entries
                .iter_mut()
                .find(|entry| entry.id == panel.active)
                .unwrap();
            active.context.git_roots = vec![shared.clone()];
            active.selected_git_root = Some(shared.clone());
            panel.entries.push(WorkspaceEntry {
                id: 2,
                manual_name: None,
                worktree_name: None,
                worktree_paths: Vec::new(),
                automatic_name: "Second".into(),
                context: WorkspaceContext {
                    git_roots: vec![shared.clone(), unique.clone()],
                    ..WorkspaceContext::default()
                },
                context_authoritative: true,
                incomplete_context_refreshes: 0,
                default_directory: None,
                selected_git_root: Some(unique.clone()),
                git_discovery: GitDiscoveryState::Authoritative,
                git: MetadataState::NotRequested,
                metadata_root: None,
                metadata_refreshed_at: None,
                restore: None,
                failed_restores: Vec::new(),
                stored: Some(StoredLayout::Leaf {
                    items: Vec::new(),
                    active: 0,
                    focused: true,
                }),
            });
            panel.next_id = 3;
            panel.reconcile_git_context(cx);
        });

        for _ in 0..100 {
            cx.run_until_parked();
            if panel.read_with(cx, |panel, _| panel.attached_worktrees.len()) == 2 {
                break;
            }
            cx.background_executor
                .timer(Duration::from_millis(10))
                .await;
        }
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.attached_worktrees.len()),
            2
        );

        opened
            .window
            .update(cx, |_, window, cx| {
                panel.update(cx, |panel, cx| panel.close_workspace(2, window, cx));
            })
            .expect("window should remain open");

        assert_eq!(
            panel.read_with(cx, |panel, _| {
                panel
                    .attached_worktrees
                    .keys()
                    .cloned()
                    .collect::<BTreeSet<_>>()
            }),
            BTreeSet::from([shared.clone()])
        );
        let project_roots = opened.workspace.read_with(cx, |workspace, cx| {
            workspace
                .project()
                .read(cx)
                .worktrees(cx)
                .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
                .collect::<BTreeSet<_>>()
        });
        assert!(project_roots.contains(&shared));
        assert!(!project_roots.contains(&unique));

        let _ = std::fs::remove_dir_all(base);
    }
}
