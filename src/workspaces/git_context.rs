//! Git context discovery and reconciliation for logical workspaces.
//!
//! Derives each workspace's Git repositories from its live terminals' working
//! directories, probes ordinary visited roots without project attachment, and
//! attaches only roots explicitly promoted to Zed's full Git integration. It
//! also bridges the vendored Zed Git panel to zmux's logical workspaces via
//! [`ZmuxRepositoryScope`].

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::{FutureExt, future::Either};
use gpui::{
    App, AppContext, Context, Entity, EntityId, Global, SharedString, Task, WeakEntity, Window,
};
use project::git_store::GitStoreEvent;
use terminal_view::TerminalView;
use workspace::item::ItemHandle;
use workspace::{Toast, Workspace, notifications::NotificationId};

use crate::metadata::{MetadataState, git_metadata_from_repository, probe_git_metadata};
use crate::notifications::WorkspaceId;

use super::persistence::{FailedRestoreSlot, StoredLayout};
use super::{WorkspaceEntry, WorkspacesPanel, is_shell_process, sanitize_process_label};

const PATH_CONTEXT_CACHE_CAPACITY: usize = 1024;
pub(super) const METADATA_PROBE_DEBOUNCE: Duration = Duration::from_millis(250);
const METADATA_PROBE_MIN_INTERVAL: Duration = Duration::from_secs(2);
const METADATA_PROBE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Debug)]
struct GitRootCacheEntry {
    root: Option<PathBuf>,
    checked_at: Instant,
    last_access: u64,
}

#[derive(Clone, Debug)]
struct CanonicalPathCacheEntry {
    identity: Option<PathBuf>,
    last_access: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum GitRootRecheck {
    Missing,
    Positive,
    Due,
    Pending(Duration),
}

/// Filesystem-derived path identity owned by one workspace panel.
///
/// Values deliberately retain the logical path supplied by the shell. The
/// canonical form exists only as a comparison key, so macOS `/var` aliases and
/// Windows verbatim paths never leak into labels or persisted sessions.
#[derive(Debug, Default)]
pub(super) struct PathContextCache {
    canonical_paths: HashMap<PathBuf, CanonicalPathCacheEntry>,
    git_roots: HashMap<PathBuf, GitRootCacheEntry>,
    access_sequence: u64,
}

impl PathContextCache {
    fn next_access(&mut self) -> u64 {
        self.access_sequence = self.access_sequence.saturating_add(1);
        self.access_sequence
    }

    fn evict_canonical_lru(&mut self, incoming: &Path) {
        if self.canonical_paths.contains_key(incoming)
            || self.canonical_paths.len() < PATH_CONTEXT_CACHE_CAPACITY
        {
            return;
        }
        if let Some(oldest) = self
            .canonical_paths
            .iter()
            .min_by_key(|(_, entry)| entry.last_access)
            .map(|(path, _)| path.clone())
        {
            self.canonical_paths.remove(&oldest);
        }
    }

    fn evict_git_root_lru(&mut self, incoming: &Path) {
        if self.git_roots.contains_key(incoming)
            || self.git_roots.len() < PATH_CONTEXT_CACHE_CAPACITY
        {
            return;
        }
        if let Some(oldest) = self
            .git_roots
            .iter()
            .min_by_key(|(_, entry)| entry.last_access)
            .map(|(path, _)| path.clone())
        {
            self.git_roots.remove(&oldest);
        }
    }

    fn cached_canonical_identity(&mut self, path: &Path) -> Option<Option<PathBuf>> {
        let access = self.next_access();
        let entry = self.canonical_paths.get_mut(path)?;
        entry.last_access = access;
        Some(entry.identity.clone())
    }

    fn insert_canonical_identity(&mut self, path: PathBuf, identity: Option<PathBuf>) {
        let access = self.next_access();
        self.evict_canonical_lru(&path);
        self.canonical_paths.insert(
            path,
            CanonicalPathCacheEntry {
                identity,
                last_access: access,
            },
        );
    }

    #[cfg(test)]
    fn canonical_identity_with(
        &mut self,
        path: &Path,
        resolve: impl FnOnce(&Path) -> std::io::Result<PathBuf>,
    ) -> Option<PathBuf> {
        let access = self.next_access();
        if let Some(entry) = self.canonical_paths.get_mut(path) {
            entry.last_access = access;
            return entry.identity.clone();
        }
        let identity = resolve(path).ok();
        self.evict_canonical_lru(path);
        self.canonical_paths.insert(
            path.to_path_buf(),
            CanonicalPathCacheEntry {
                identity: identity.clone(),
                last_access: access,
            },
        );
        identity
    }

    fn paths_match_cached(&mut self, left: &Path, right: &Path) -> bool {
        if left == right {
            return true;
        }
        let left_identity = self.cached_canonical_identity(left).flatten();
        let right_identity = self.cached_canonical_identity(right).flatten();
        left_identity.as_deref() == Some(right)
            || right_identity.as_deref() == Some(left)
            || left_identity
                .zip(right_identity)
                .is_some_and(|(left, right)| left == right)
    }

    fn cached_nearest_git_root(&mut self, directory: &Path) -> Option<Option<PathBuf>> {
        let access = self.next_access();
        let entry = self.git_roots.get_mut(directory)?;
        entry.last_access = access;
        Some(entry.root.clone())
    }

    fn insert_git_root(&mut self, directory: PathBuf, root: Option<PathBuf>, checked_at: Instant) {
        let access = self.next_access();
        self.evict_git_root_lru(&directory);
        self.git_roots.insert(
            directory,
            GitRootCacheEntry {
                root,
                checked_at,
                last_access: access,
            },
        );
    }

    #[cfg(test)]
    fn nearest_git_root_with(
        &mut self,
        directory: &Path,
        is_git_root: impl Fn(&Path) -> bool,
    ) -> Option<PathBuf> {
        let access = self.next_access();
        if let Some(cached) = self.git_roots.get_mut(directory) {
            cached.last_access = access;
            return cached.root.clone();
        }
        let root = directory
            .ancestors()
            .find(|ancestor| is_git_root(ancestor))
            .map(Path::to_path_buf);
        self.evict_git_root_lru(directory);
        self.git_roots.insert(
            directory.to_path_buf(),
            GitRootCacheEntry {
                root: root.clone(),
                checked_at: Instant::now(),
                last_access: access,
            },
        );
        root
    }

    pub(super) fn git_root_recheck(
        &mut self,
        directory: &Path,
        interval: Duration,
    ) -> GitRootRecheck {
        let access = self.next_access();
        let Some(cached) = self.git_roots.get_mut(directory) else {
            return GitRootRecheck::Missing;
        };
        cached.last_access = access;
        if cached.root.is_some() {
            return GitRootRecheck::Positive;
        }
        let elapsed = cached.checked_at.elapsed();
        if elapsed >= interval {
            self.git_roots.remove(directory);
            GitRootRecheck::Due
        } else {
            GitRootRecheck::Pending(interval - elapsed)
        }
    }

    fn invalidate_root(&mut self, root: &Path) -> Vec<PathBuf> {
        let mut identities = vec![root.to_path_buf()];
        if let Some(entry) = self.canonical_paths.get(root)
            && let Some(identity) = entry.identity.as_ref()
            && identity != root
        {
            identities.push(identity.clone());
        }
        let belongs_to_root = |path: &Path| {
            identities
                .iter()
                .any(|identity| path == identity || path.starts_with(identity))
        };

        self.canonical_paths.retain(|logical, entry| {
            !belongs_to_root(logical) && !entry.identity.as_deref().is_some_and(&belongs_to_root)
        });
        let removed = self
            .git_roots
            .iter()
            .filter_map(|(directory, entry)| {
                (belongs_to_root(directory) || entry.root.as_deref().is_some_and(&belongs_to_root))
                    .then_some(directory.clone())
            })
            .collect::<Vec<_>>();
        for directory in &removed {
            self.git_roots.remove(directory);
        }
        removed
    }

    #[cfg(test)]
    fn clear(&mut self) {
        self.canonical_paths.clear();
        self.git_roots.clear();
    }
}

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
    pub(super) blocked_git_roots: Vec<PathBuf>,
    pub(super) git_root: Option<PathBuf>,
    pub(super) foreground_processes: Vec<String>,
    pub(super) shell_count: usize,
    pub(super) reported_directories: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum IndexRootBlockReason {
    NotGitRepository,
    ProtectedLocation,
    Unverifiable,
}

impl IndexRootBlockReason {
    fn message(self) -> &'static str {
        match self {
            Self::NotGitRepository => "it is not an exact Git repository root",
            Self::ProtectedLocation => {
                "it contains your home directory or Zmux's own data directory"
            }
            Self::Unverifiable => "its canonical location could not be verified",
        }
    }
}

fn index_root_block_reason(
    root: &Path,
    cache: &Mutex<PathContextCache>,
) -> Option<IndexRootBlockReason> {
    let mut cache = cache.lock().unwrap_or_else(|poisoned| {
        log::error!("recovering poisoned path context cache during admission check");
        poisoned.into_inner()
    });
    let Some(discovered_root) = cache.cached_nearest_git_root(root) else {
        return Some(IndexRootBlockReason::Unverifiable);
    };
    let Some(discovered_root) = discovered_root else {
        return Some(IndexRootBlockReason::NotGitRepository);
    };
    if !cache.paths_match_cached(root, &discovered_root) {
        return Some(IndexRootBlockReason::NotGitRepository);
    }
    let Some(root) = cache.cached_canonical_identity(root).flatten() else {
        return Some(IndexRootBlockReason::Unverifiable);
    };
    if root.parent().is_none() {
        return Some(IndexRootBlockReason::ProtectedLocation);
    }

    for protected in [paths::home_dir().as_path(), paths::data_dir().as_path()] {
        let protected = cache
            .cached_canonical_identity(protected)
            .flatten()
            .unwrap_or_else(|| protected.to_path_buf());
        if protected.starts_with(&root) {
            return Some(IndexRootBlockReason::ProtectedLocation);
        }
    }
    None
}

#[cfg(test)]
fn index_root_block_reason_with(
    root: &Path,
    home: &Path,
    data_dir: &Path,
    is_git_root: impl Fn(&Path) -> bool,
    canonicalize: impl Fn(&Path) -> std::io::Result<PathBuf>,
) -> Option<IndexRootBlockReason> {
    if !is_git_root(root) {
        return Some(IndexRootBlockReason::NotGitRepository);
    }
    let Ok(root) = canonicalize(root) else {
        return Some(IndexRootBlockReason::Unverifiable);
    };
    if root.parent().is_none() {
        return Some(IndexRootBlockReason::ProtectedLocation);
    }

    for protected in [home, data_dir] {
        let protected = canonicalize(protected).unwrap_or_else(|_| protected.to_path_buf());
        if protected.starts_with(&root) {
            return Some(IndexRootBlockReason::ProtectedLocation);
        }
    }
    None
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

#[cfg(test)]
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

fn git_root_is_desired(
    entries: &[WorkspaceEntry],
    root: &Path,
    cache: &Mutex<PathContextCache>,
) -> bool {
    entries.iter().any(|entry| {
        entry
            .worktree_paths
            .iter()
            .chain(entry.promoted_git_roots.iter())
            .any(|candidate| paths_match(cache, candidate, root))
    })
}

fn plan_git_root_reconciliation(
    reference_counts: BTreeMap<PathBuf, usize>,
    attached: &BTreeSet<PathBuf>,
    pending: &BTreeSet<PathBuf>,
    cache: &Mutex<PathContextCache>,
) -> GitRootReconciliation {
    let mut desired: Vec<PathBuf> = Vec::new();
    for root in reference_counts.keys() {
        push_unique_logical_path(&mut desired, root.clone(), cache);
    }

    let mut retained: Vec<PathBuf> = Vec::new();
    let mut removed = BTreeSet::new();
    for root in attached {
        let referenced = desired
            .iter()
            .any(|desired| paths_match(cache, desired, root));
        let duplicate = retained
            .iter()
            .any(|existing| paths_match(cache, existing, root));
        if referenced && !duplicate {
            retained.push(root.clone());
        } else {
            removed.insert(root.clone());
        }
    }

    let added = desired
        .into_iter()
        .filter(|root| {
            !retained
                .iter()
                .chain(pending)
                .any(|existing| paths_match(cache, existing, root))
        })
        .collect();
    GitRootReconciliation { added, removed }
}

fn push_unique_logical_path(
    paths: &mut Vec<PathBuf>,
    path: PathBuf,
    cache: &Mutex<PathContextCache>,
) {
    if !paths
        .iter()
        .any(|existing| paths_match(cache, existing, &path))
    {
        paths.push(path);
    }
}

fn matched_logical_git_root(
    discovered: &[PathBuf],
    candidate: &Path,
    cache: &Mutex<PathContextCache>,
) -> Option<PathBuf> {
    discovered
        .iter()
        .find(|root| paths_match(cache, root, candidate))
        .cloned()
}

fn reconcile_selected_git_root(
    selected: &mut Option<PathBuf>,
    pinned: Option<&Path>,
    discovered_roots: &[PathBuf],
    discovery: GitDiscoveryState,
    cache: &Mutex<PathContextCache>,
) {
    if let Some(pinned) = pinned {
        *selected = Some(
            matched_logical_git_root(discovered_roots, pinned, cache)
                .unwrap_or_else(|| pinned.to_path_buf()),
        );
        return;
    }
    if discovery != GitDiscoveryState::Authoritative {
        return;
    }
    if let Some(matched) = selected.as_ref().and_then(|root| {
        discovered_roots
            .iter()
            .find(|discovered| paths_match(cache, root, discovered))
    }) {
        if selected.as_ref() != Some(matched) {
            *selected = Some(matched.clone());
        }
    } else {
        *selected = discovered_roots.first().cloned();
    }
}

fn promote_git_root(
    selected: &mut Option<PathBuf>,
    pinned: &mut Option<PathBuf>,
    promoted: &mut VecDeque<PathBuf>,
    root: PathBuf,
    cache: &Mutex<PathContextCache>,
) {
    promoted.retain(|candidate| !paths_match(cache, candidate, &root));
    promoted.push_front(root.clone());
    promoted.truncate(super::MAX_PROMOTED_GIT_ROOTS_PER_WORKSPACE);
    *selected = Some(root.clone());
    *pinned = Some(root);
}

fn desired_git_root_reference_counts(
    entries: &[WorkspaceEntry],
    cache: &Mutex<PathContextCache>,
) -> BTreeMap<PathBuf, usize> {
    let mut counts = BTreeMap::new();
    for entry in entries {
        for root in workspace_attachment_roots(entry, cache) {
            *counts.entry(root).or_insert(0) += 1;
        }
    }
    counts
}

fn workspace_attachment_roots(
    entry: &WorkspaceEntry,
    cache: &Mutex<PathContextCache>,
) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for root in entry
        .worktree_paths
        .iter()
        .chain(entry.promoted_git_roots.iter())
    {
        if index_root_block_reason(root, cache).is_none() {
            push_unique_logical_path(&mut roots, root.clone(), cache);
        }
    }
    roots
}

fn track_pending_worktree(pending: &mut BTreeSet<PathBuf>, root: PathBuf) -> bool {
    pending.insert(root)
}

fn metadata_probe_is_current(
    current_root: Option<&Path>,
    current_generation: u64,
    requested_root: &Path,
    requested_generation: u64,
) -> bool {
    current_generation == requested_generation && current_root == Some(requested_root)
}

pub(super) fn accept_workspace_context_probe_result(
    current_generation: Option<u64>,
    requested_generation: u64,
    observed: WorkspaceContext,
) -> Option<WorkspaceContext> {
    (current_generation == Some(requested_generation)).then_some(observed)
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
            .unwrap_or_else(|poisoned| {
                log::error!("recovering poisoned repository scope registry");
                poisoned.into_inner()
            })
            .insert(project.entity_id(), panel.downgrade());
    }

    fn panel_for(&self, project: &Entity<project::Project>) -> Option<WeakEntity<WorkspacesPanel>> {
        self.panels
            .lock()
            .unwrap_or_else(|poisoned| {
                log::error!("recovering poisoned repository scope registry");
                poisoned.into_inner()
            })
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
        let panel = self.panel_for(project).and_then(|panel| panel.upgrade());
        let roots = panel
            .as_ref()
            .map(|panel| panel.read(cx).active_attachment_roots())
            .unwrap_or_default();
        project
            .read(cx)
            .git_store()
            .read(cx)
            .repositories()
            .values()
            .filter(|repo| {
                roots.iter().any(|root| {
                    panel.as_ref().is_some_and(|panel| {
                        paths_match(
                            &panel.read(cx).path_context_cache,
                            repo.read(cx).snapshot().work_directory_abs_path.as_ref(),
                            root,
                        )
                    })
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
    /// Keep metadata for explicitly attached roots synchronized with Zed's
    /// repository model. Unattached roots use bounded, event-driven probes.
    pub(super) fn subscribe_to_git_metadata(workspace: &Entity<Workspace>, cx: &mut Context<Self>) {
        let git_store = workspace.read(cx).project().read(cx).git_store().clone();
        cx.subscribe(&git_store, |this, _git_store, event, cx| match event {
            GitStoreEvent::RepositoryAdded | GitStoreEvent::RepositoryRemoved(_) => {
                this.schedule_context_refresh(cx);
                this.request_metadata_refreshes(cx);
            }
            GitStoreEvent::RepositoryUpdated(_, _, _)
            | GitStoreEvent::ActiveRepositoryChanged(_) => {
                this.request_metadata_refreshes(cx);
            }
            GitStoreEvent::IndexWriteError(_)
            | GitStoreEvent::JobsUpdated
            | GitStoreEvent::ConflictsUpdated
            | GitStoreEvent::GlobalConfigurationUpdated => {}
        })
        .detach();
    }

    fn active_attachment_roots(&self) -> Vec<PathBuf> {
        self.entries
            .iter()
            .find(|entry| entry.id == self.active)
            .map(|entry| workspace_attachment_roots(entry, &self.path_context_cache))
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
                        entry.default_directory.as_deref().and_then(|directory| {
                            nearest_git_root(&self.path_context_cache, directory)
                        })
                    })
            })
            .filter(|root| index_root_block_reason(root, &self.path_context_cache).is_none())
    }

    fn open_git_roots(&self) -> Vec<PathBuf> {
        let mut roots = Vec::new();
        for entry in &self.entries {
            for root in entry
                .context
                .git_roots
                .iter()
                .chain(entry.selected_git_root.iter())
                .chain(entry.pinned_git_root.iter())
                .chain(entry.promoted_git_roots.iter())
                .chain(&entry.worktree_paths)
                .cloned()
            {
                if index_root_block_reason(&root, &self.path_context_cache).is_none() {
                    push_unique_logical_path(&mut roots, root, &self.path_context_cache);
                }
            }
            if let Some(root) = entry
                .default_directory
                .as_deref()
                .and_then(|directory| nearest_git_root(&self.path_context_cache, directory))
                && index_root_block_reason(&root, &self.path_context_cache).is_none()
            {
                push_unique_logical_path(&mut roots, root, &self.path_context_cache);
            }
        }
        roots.sort();
        roots
    }

    fn select_git_root(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        self.pin_git_root_for_workspace(self.active, root, cx);
    }

    pub(super) fn active_git_root_choices(&self) -> Vec<PathBuf> {
        let Some(entry) = self.entries.iter().find(|entry| entry.id == self.active) else {
            return Vec::new();
        };
        let mut roots = Vec::new();
        for root in entry
            .pinned_git_root
            .iter()
            .chain(entry.selected_git_root.iter())
            .chain(entry.context.git_roots.iter())
            .chain(entry.promoted_git_roots.iter())
            .chain(entry.worktree_paths.iter())
        {
            if index_root_block_reason(root, &self.path_context_cache).is_none() {
                push_unique_logical_path(&mut roots, root.clone(), &self.path_context_cache);
            }
        }
        roots.sort();
        roots
    }

    pub(super) fn active_git_root_is_pinned(&self) -> bool {
        self.entries
            .iter()
            .find(|entry| entry.id == self.active)
            .is_some_and(|entry| entry.pinned_git_root.is_some())
    }

    pub(super) fn active_git_root_attachment_pending(&self) -> bool {
        self.active_git_root().is_some_and(|root| {
            self.pending_worktrees
                .iter()
                .any(|pending| paths_match(&self.path_context_cache, pending, &root))
        })
    }

    pub(super) fn pin_active_git_root(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        self.pin_git_root_for_workspace(self.active, root, cx);
    }

    fn pin_git_root_for_workspace(
        &mut self,
        workspace_id: WorkspaceId,
        root: PathBuf,
        cx: &mut Context<Self>,
    ) {
        if index_root_block_reason(&root, &self.path_context_cache).is_some() {
            return;
        }
        let Some(index) = self
            .entries
            .iter()
            .position(|entry| entry.id == workspace_id)
        else {
            return;
        };
        let logical_root = {
            let entry = &self.entries[index];
            entry
                .context
                .git_roots
                .iter()
                .chain(entry.worktree_paths.iter())
                .find(|candidate| paths_match(&self.path_context_cache, candidate, &root))
                .cloned()
                .unwrap_or(root)
        };
        let entry = &mut self.entries[index];
        promote_git_root(
            &mut entry.selected_git_root,
            &mut entry.pinned_git_root,
            &mut entry.promoted_git_roots,
            logical_root,
            &self.path_context_cache,
        );
        entry.metadata_checked_at = None;
        self.reconcile_git_context(cx);
        self.request_metadata_refreshes(cx);
        self.schedule_session_persistence(cx);
        cx.notify();
    }

    pub(super) fn follow_terminal_git_root(&mut self, cx: &mut Context<Self>) {
        let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| entry.id == self.active)
        else {
            return;
        };
        entry.pinned_git_root = None;
        entry.promoted_git_roots.clear();
        reconcile_selected_git_root(
            &mut entry.selected_git_root,
            None,
            &entry.context.git_roots,
            entry.git_discovery,
            &self.path_context_cache,
        );
        entry.metadata_checked_at = None;
        self.reconcile_git_context(cx);
        self.request_metadata_refreshes(cx);
        self.schedule_session_persistence(cx);
        cx.notify();
    }

    pub(super) fn workspace_id_for_git_root(&self, path: &Path) -> Option<WorkspaceId> {
        self.entries.iter().find_map(|entry| {
            (entry
                .selected_git_root
                .as_deref()
                .is_some_and(|root| paths_match(&self.path_context_cache, root, path))
                || entry
                    .default_directory
                    .as_deref()
                    .is_some_and(|root| paths_match(&self.path_context_cache, root, path))
                || entry
                    .worktree_paths
                    .iter()
                    .any(|root| paths_match(&self.path_context_cache, root, path))
                || entry
                    .context
                    .git_roots
                    .iter()
                    .any(|root| paths_match(&self.path_context_cache, root, path)))
            .then_some(entry.id)
        })
    }

    pub(super) fn workspace_id_for_directory(&self, path: &Path) -> Option<WorkspaceId> {
        self.entries.iter().find_map(|entry| {
            (entry
                .default_directory
                .as_deref()
                .is_some_and(|directory| paths_match(&self.path_context_cache, directory, path))
                || entry
                    .worktree_paths
                    .iter()
                    .any(|directory| paths_match(&self.path_context_cache, directory, path))
                || entry
                    .context
                    .working_directories
                    .iter()
                    .any(|directory| paths_match(&self.path_context_cache, directory, path)))
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
            let restored_shell_count = entry
                .restore
                .as_ref()
                .map(layout_snapshot_terminal_count)
                .unwrap_or_default();
            if entry.context_authoritative && entry.context.shell_count >= restored_shell_count {
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
                entry.pinned_git_root.as_deref(),
                &entry.context.git_roots,
                entry.git_discovery,
                &self.path_context_cache,
            );
        }

        self.sync_blocked_root_notifications(cx);

        let reference_counts =
            desired_git_root_reference_counts(&self.entries, &self.path_context_cache);
        let attached = self.attached_worktrees.keys().cloned().collect();
        let reconciliation = plan_git_root_reconciliation(
            reference_counts,
            &attached,
            &self.pending_worktrees,
            &self.path_context_cache,
        );
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let project = workspace.read(cx).project().clone();

        for root in reconciliation.removed {
            if let Some(worktree) = self.attached_worktrees.remove(&root) {
                let id = worktree.read(cx).id();
                project.update(cx, |project, cx| project.remove_worktree(id, cx));
                self.invalidate_path_context_for_root(&root);
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
            let existing = project.read(cx).worktrees(cx).find(|worktree| {
                paths_match(
                    &self.path_context_cache,
                    worktree.read(cx).abs_path().as_ref(),
                    &root,
                )
            });
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
                            if git_root_is_desired(
                                &this.entries,
                                &root,
                                &this.path_context_cache,
                            ) =>
                        {
                            this.attached_worktrees.insert(root.clone(), worktree);
                            // Attaching a Project worktree does not change the
                            // repository on disk. Keep the background-probed
                            // admission identities warm; clearing them here
                            // would make the next cache-only reconciliation
                            // classify this valid root as unverifiable and
                            // immediately detach it again.
                            this.activate_selected_repository(cx);
                            this.request_metadata_refreshes(cx);
                        }
                        Ok(worktree) => {
                            // Repository discovery moved on while this worktree
                            // was scanning. Remove the Project's ownership as
                            // well as dropping this late result.
                            let id = worktree.read(cx).id();
                            project.update(cx, |project, cx| project.remove_worktree(id, cx));
                            this.invalidate_path_context_for_root(&root);
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

    pub(super) fn schedule_worktree_admission_audit(
        &mut self,
        project: Entity<project::Project>,
        id: project::WorktreeId,
        cx: &mut Context<Self>,
    ) {
        let panel = cx.weak_entity();
        cx.defer(move |cx| {
            panel
                .update(cx, |panel, cx| {
                    panel.audit_added_worktree(&project, id, cx);
                })
                .ok();
        });
    }

    fn audit_added_worktree(
        &mut self,
        project: &Entity<project::Project>,
        id: project::WorktreeId,
        cx: &mut Context<Self>,
    ) {
        let Some(worktree) = project.read(cx).worktree_for_id(id, cx) else {
            return;
        };
        let (root, is_directory) = {
            let worktree = worktree.read(cx);
            (
                worktree.abs_path().to_path_buf(),
                worktree
                    .root_entry()
                    .map_or_else(|| worktree.abs_path().is_dir(), |entry| entry.is_dir()),
            )
        };
        if !is_directory {
            return;
        }

        if let Some(reason) = index_root_block_reason(&root, &self.path_context_cache) {
            log::warn!(
                "removing inadmissible Zmux worktree {}: {}",
                root.display(),
                reason.message()
            );
            self.audited_blocked_roots.insert(root.clone());
            self.show_blocked_root_notification(root, reason, cx);
            project.update(cx, |project, cx| project.remove_worktree(id, cx));
        } else {
            if self.audited_blocked_roots.remove(&root) {
                let notification_id = blocked_root_notification_id(&root);
                if let Some(workspace) = self.workspace.upgrade() {
                    workspace.update(cx, |workspace, cx| {
                        workspace.dismiss_notification(&notification_id, cx);
                    });
                }
                self.warned_scan_roots.remove(&root);
            }
            log::info!("admitted Zmux Git worktree {}", root.display());
        }
    }

    fn sync_blocked_root_notifications(&mut self, cx: &mut Context<Self>) {
        let mut referenced = self
            .entries
            .iter()
            .flat_map(|entry| entry.context.blocked_git_roots.iter().cloned())
            .collect::<BTreeSet<_>>();
        referenced.extend(self.audited_blocked_roots.iter().cloned());

        let removed = self
            .warned_scan_roots
            .difference(&referenced)
            .cloned()
            .collect::<Vec<_>>();
        if let Some(workspace) = self.workspace.upgrade() {
            for root in removed {
                let id = blocked_root_notification_id(&root);
                workspace.update(cx, |workspace, cx| {
                    workspace.dismiss_notification(&id, cx);
                });
                self.warned_scan_roots.remove(&root);
            }
        }

        for root in referenced {
            if self.warned_scan_roots.contains(&root) {
                continue;
            }
            let reason = index_root_block_reason(&root, &self.path_context_cache)
                .unwrap_or(IndexRootBlockReason::Unverifiable);
            self.show_blocked_root_notification(root, reason, cx);
        }
    }

    fn show_blocked_root_notification(
        &mut self,
        root: PathBuf,
        reason: IndexRootBlockReason,
        cx: &mut Context<Self>,
    ) {
        if !self.warned_scan_roots.insert(root.clone()) {
            return;
        }
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let panel = cx.weak_entity();
        let message = format!(
            "Git indexing is disabled for {} because {}. Terminals remain fully usable.",
            root.display(),
            reason.message()
        );
        workspace.update(cx, |workspace, cx| {
            workspace.show_toast(
                Toast::new(blocked_root_notification_id(&root), message).on_click(
                    "Choose a narrower folder",
                    move |window, cx| {
                        if let Some(panel) = panel.upgrade() {
                            panel.update(cx, |panel, cx| {
                                panel.prompt_for_workspace(window, cx);
                            });
                        }
                    },
                ),
                cx,
            );
        });
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
        self.schedule_session_persistence(cx);
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
        self.schedule_session_persistence(cx);
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
                    &self.path_context_cache,
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
        let repositories = self
            .workspace
            .upgrade()
            .map(|workspace| workspace.read(cx).project().read(cx).git_store().clone())
            .map(|git_store| {
                git_store
                    .read(cx)
                    .repositories()
                    .values()
                    .map(|repository| repository.read(cx).snapshot())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let mut changed = false;
        let mut probes = Vec::new();
        let mut cancelled_probes = Vec::new();
        for entry in &mut self.entries {
            let root = entry.selected_git_root.clone();
            let root_changed = entry.metadata_root != root;
            if root_changed {
                entry.metadata_generation = entry.metadata_generation.wrapping_add(1);
                entry.metadata_root = root.clone();
                entry.metadata_checked_at = None;
                cancelled_probes.push(entry.id);
            }
            let repository = root.as_ref().and_then(|root| {
                repositories.iter().find(|repository| {
                    paths_match(
                        &self.path_context_cache,
                        repository.work_directory_abs_path.as_ref(),
                        root,
                    )
                })
            });
            let git = match (root.as_ref(), repository) {
                (_, Some(repository)) => {
                    entry.metadata_generation = entry.metadata_generation.wrapping_add(1);
                    entry.metadata_checked_at = Some(Instant::now());
                    cancelled_probes.push(entry.id);
                    git_metadata_from_repository(Some(repository))
                }
                (Some(_), None) if root_changed => {
                    probes.push(entry.id);
                    MetadataState::NotRequested
                }
                (Some(_), None) => {
                    if matches!(entry.git, MetadataState::NotRequested) {
                        probes.push(entry.id);
                    }
                    entry.git.clone()
                }
                (None, None) => MetadataState::NotRequested,
            };
            if entry.git != git {
                entry.git = git;
                changed = true;
            }
        }
        if changed {
            cx.notify();
        }
        for workspace_id in cancelled_probes {
            self.metadata_probe_tasks.remove(&workspace_id);
        }
        for workspace_id in probes {
            self.schedule_lightweight_metadata_probe(workspace_id, Duration::ZERO, cx);
        }
    }

    pub(super) fn schedule_lightweight_metadata_probe(
        &mut self,
        workspace_id: WorkspaceId,
        requested_delay: Duration,
        cx: &mut Context<Self>,
    ) {
        if self.metadata_probe_tasks.contains_key(&workspace_id) {
            return;
        }
        let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| entry.id == workspace_id)
        else {
            return;
        };
        let Some(root) = entry.selected_git_root.clone() else {
            return;
        };
        if self
            .attached_worktrees
            .keys()
            .any(|attached| paths_match(&self.path_context_cache, attached, &root))
        {
            return;
        }

        let cooldown = entry
            .metadata_checked_at
            .and_then(|checked_at| METADATA_PROBE_MIN_INTERVAL.checked_sub(checked_at.elapsed()))
            .unwrap_or_default();
        let delay = requested_delay.max(cooldown);
        entry.metadata_generation = entry.metadata_generation.wrapping_add(1);
        let generation = entry.metadata_generation;
        let executor = cx.background_executor().clone();
        let probe_root = root.clone();
        let background = cx.background_spawn(async move {
            executor.timer(delay).await;
            let probe = probe_git_metadata(&probe_root).boxed();
            let timeout = executor.timer(METADATA_PROBE_TIMEOUT).boxed();
            match futures::future::select(probe, timeout).await {
                Either::Left((result, _)) => result,
                Either::Right(((), _)) => Err("git status probe timed out".to_string()),
            }
        });
        let task = cx.spawn(async move |this, cx| {
            let result = background.await;
            this.update(cx, |this, cx| {
                this.metadata_probe_tasks.remove(&workspace_id);
                let Some(entry) = this
                    .entries
                    .iter_mut()
                    .find(|entry| entry.id == workspace_id)
                else {
                    return;
                };
                if !metadata_probe_is_current(
                    entry.metadata_root.as_deref(),
                    entry.metadata_generation,
                    &root,
                    generation,
                ) {
                    return;
                }
                entry.metadata_checked_at = Some(Instant::now());
                let next = match result {
                    Ok(metadata) => MetadataState::Ready(metadata),
                    Err(error) => MetadataState::Unavailable(error),
                };
                if entry.git != next {
                    entry.git = next;
                    cx.notify();
                }
            })
            .ok();
        });
        self.metadata_probe_tasks.insert(workspace_id, task);
    }

    fn invalidate_path_context_for_root(&mut self, root: &Path) {
        let removed = self
            .path_context_cache
            .lock()
            .unwrap_or_else(|poisoned| {
                log::error!("recovering poisoned path context cache during invalidation");
                poisoned.into_inner()
            })
            .invalidate_root(root);
        for directory in removed {
            self.git_root_recheck_schedule.remove(&directory);
        }
    }
}

fn layout_snapshot_terminal_count(snapshot: &crate::session::LayoutSnapshot) -> usize {
    fn count(node: &crate::session::LayoutNodeSnapshot) -> usize {
        match node {
            crate::session::LayoutNodeSnapshot::Leaf { tabs, .. } => tabs.len(),
            crate::session::LayoutNodeSnapshot::Split { first, second, .. } => {
                count(first) + count(second)
            }
        }
    }

    count(&snapshot.root)
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
    context
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
    context
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

pub(super) fn finalize_workspace_context(
    mut context: WorkspaceContext,
    cache: &Mutex<PathContextCache>,
) -> WorkspaceContext {
    // Admission checks on the foreground path are cache-only. Warm protected
    // identities here alongside cwd/root identities so symlink aliases cannot
    // bypass the protected-root policy without reintroducing UI-thread I/O.
    context.working_directories.sort();
    context.working_directories.dedup();
    for directory in &context.working_directories {
        probe_canonical_identity(cache, directory);
    }
    probe_canonical_identity(cache, paths::home_dir().as_path());
    probe_canonical_identity(cache, paths::data_dir().as_path());

    let processes = context
        .foreground_processes
        .drain(..)
        .collect::<BTreeSet<_>>();
    context.foreground_processes = processes.into_iter().take(8).collect();

    let mut roots: Vec<PathBuf> = Vec::new();
    let mut blocked_roots: Vec<PathBuf> = Vec::new();
    for root in context
        .working_directories
        .iter()
        .filter_map(|directory| probe_nearest_git_root(cache, directory))
    {
        // The ancestor lookup caches `directory -> root`; admission asks the
        // distinct question `root -> root`, so seed that exact entry too.
        probe_nearest_git_root(cache, &root);
        probe_canonical_identity(cache, &root);
        let destination = if index_root_block_reason(&root, cache).is_some() {
            &mut blocked_roots
        } else {
            &mut roots
        };
        if !destination
            .iter()
            .any(|existing| probe_paths_match(cache, existing, &root))
        {
            destination.push(root);
        }
    }
    roots.sort();
    blocked_roots.sort();
    context.git_roots = roots;
    context.blocked_git_roots = blocked_roots;
    if context.git_roots.len() == 1 {
        context.git_root = context.git_roots.first().cloned();
    }
    context
}

/// Warm cache-only foreground admission for explicit workspace roots without
/// adding those paths to the terminal-derived context.
pub(super) fn warm_workspace_git_root_candidates(
    candidates: &[PathBuf],
    cache: &Mutex<PathContextCache>,
) {
    probe_canonical_identity(cache, paths::home_dir().as_path());
    probe_canonical_identity(cache, paths::data_dir().as_path());
    for candidate in candidates {
        probe_canonical_identity(cache, candidate);
        if let Some(root) = probe_nearest_git_root(cache, candidate) {
            probe_nearest_git_root(cache, &root);
            probe_canonical_identity(cache, &root);
        }
    }
}

/// Resolve a Git root on a background executor without retaining the shared
/// cache lock across filesystem calls. A wedged mount can delay only the probe
/// that touched it; foreground cache readers remain responsive.
fn probe_nearest_git_root(cache: &Mutex<PathContextCache>, directory: &Path) -> Option<PathBuf> {
    let cached = cache
        .lock()
        .unwrap_or_else(|poisoned| {
            log::error!("recovering poisoned path context cache during Git-root lookup");
            poisoned.into_inner()
        })
        .cached_nearest_git_root(directory);
    if let Some(root) = cached {
        return root;
    }

    let root = directory
        .ancestors()
        .find(|ancestor| ancestor.join(".git").exists())
        .map(Path::to_path_buf);
    cache
        .lock()
        .unwrap_or_else(|poisoned| {
            log::error!("recovering poisoned path context cache after Git-root lookup");
            poisoned.into_inner()
        })
        .insert_git_root(directory.to_path_buf(), root.clone(), Instant::now());
    root
}

fn probe_canonical_identity(cache: &Mutex<PathContextCache>, path: &Path) -> Option<PathBuf> {
    let cached = cache
        .lock()
        .unwrap_or_else(|poisoned| {
            log::error!("recovering poisoned path context cache during canonical lookup");
            poisoned.into_inner()
        })
        .cached_canonical_identity(path);
    if let Some(identity) = cached {
        return identity;
    }

    let identity = std::fs::canonicalize(path).ok();
    cache
        .lock()
        .unwrap_or_else(|poisoned| {
            log::error!("recovering poisoned path context cache after canonical lookup");
            poisoned.into_inner()
        })
        .insert_canonical_identity(path.to_path_buf(), identity.clone());
    identity
}

fn probe_paths_match(cache: &Mutex<PathContextCache>, left: &Path, right: &Path) -> bool {
    left == right
        || probe_canonical_identity(cache, left)
            .zip(probe_canonical_identity(cache, right))
            .is_some_and(|(left, right)| left == right)
}

fn nearest_git_root(cache: &Mutex<PathContextCache>, directory: &Path) -> Option<PathBuf> {
    cache
        .lock()
        .unwrap_or_else(|poisoned| {
            log::error!("recovering poisoned path context cache during Git-root lookup");
            poisoned.into_inner()
        })
        .cached_nearest_git_root(directory)
        .flatten()
}

fn blocked_root_notification_id(root: &Path) -> NotificationId {
    NotificationId::named(format!("zmux-blocked-index-root:{}", root.display()).into())
}

/// Compare existing filesystem paths without replacing their user-visible form.
///
/// `canonicalize` rewrites `/var` to `/private/var` on macOS and may introduce
/// verbatim (`\\?\`) or long-name paths on Windows. Those are useful identity
/// keys, but persisting them changes workspace labels and breaks equality with
/// shell-reported paths, so canonicalization is confined to comparison.
fn paths_match(cache: &Mutex<PathContextCache>, left: &Path, right: &Path) -> bool {
    cache
        .lock()
        .unwrap_or_else(|poisoned| {
            log::error!("recovering poisoned path context cache during path comparison");
            poisoned.into_inner()
        })
        .paths_match_cached(left, right)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use crate::session::{LayoutNodeSnapshot, LayoutSnapshot, TerminalSnapshot};

    #[test]
    fn poisoned_path_context_cache_degrades_without_panicking() {
        let cache = Arc::new(Mutex::new(PathContextCache::default()));
        let poisoned = cache.clone();
        assert!(
            std::thread::spawn(move || {
                let _guard = poisoned.lock().unwrap();
                panic!("poison path context cache");
            })
            .join()
            .is_err()
        );

        assert_eq!(
            nearest_git_root(&cache, Path::new("/path/that/does/not/exist")),
            None
        );
        assert!(!paths_match(
            &cache,
            Path::new("/path/that/does/not/exist"),
            Path::new("/another/missing/path")
        ));
    }

    /// `temp_dir()` sits behind the `/var -> /private/var` symlink on macOS,
    /// while live shells report physical cwds. Tests that assert exact paths
    /// against live-derived context must start from the physical temp root.
    /// Other platforms keep the logical form: on Windows `canonicalize`
    /// produces verbatim (`\\?\`) paths that shells never report back.
    fn physical_temp_dir() -> PathBuf {
        let temp = std::env::temp_dir();
        if cfg!(target_os = "macos") {
            temp.canonicalize().unwrap_or(temp)
        } else {
            temp
        }
    }

    fn path_cache() -> Mutex<PathContextCache> {
        Mutex::new(PathContextCache::default())
    }

    async fn initialize_zmux(cx: &mut gpui::TestAppContext) {
        let initialization = cx.update(crate::app::init_zmux);
        initialization.await;
    }

    async fn warm_git_context_cache(
        panel: &Entity<WorkspacesPanel>,
        directories: Vec<PathBuf>,
        cx: &mut gpui::TestAppContext,
    ) {
        let cache = panel.read_with(cx, |panel, _| panel.path_context_cache.clone());
        let probe = cx.update(|cx| {
            cx.background_spawn(async move {
                finalize_workspace_context(
                    WorkspaceContext {
                        working_directories: directories,
                        ..WorkspaceContext::default()
                    },
                    cache.as_ref(),
                )
            })
        });
        let _ = probe.await;
    }

    #[test]
    fn index_root_policy_requires_an_exact_git_root() {
        let root = Path::new("/users/me/project");
        assert_eq!(
            index_root_block_reason_with(
                root,
                Path::new("/users/me"),
                Path::new("/users/me/.local/share/zmux"),
                |_| false,
                |path| Ok(path.to_path_buf()),
            ),
            Some(IndexRootBlockReason::NotGitRepository)
        );
    }

    #[test]
    fn index_root_policy_blocks_filesystem_home_and_data_ancestors() {
        let home = Path::new("/users/me");
        let data = Path::new("/users/me/.local/share/zmux");
        let classify = |root: &Path| {
            index_root_block_reason_with(root, home, data, |_| true, |path| Ok(path.to_path_buf()))
        };

        assert_eq!(
            classify(Path::new("/")),
            Some(IndexRootBlockReason::ProtectedLocation)
        );
        assert_eq!(
            classify(home),
            Some(IndexRootBlockReason::ProtectedLocation)
        );
        assert_eq!(
            classify(Path::new("/users")),
            Some(IndexRootBlockReason::ProtectedLocation)
        );
        assert_eq!(
            classify(Path::new("/users/me/.local")),
            Some(IndexRootBlockReason::ProtectedLocation)
        );
        assert_eq!(classify(Path::new("/users/me/project")), None);
    }

    #[test]
    fn index_root_policy_fails_closed_when_canonicalization_fails() {
        assert_eq!(
            index_root_block_reason_with(
                Path::new("/users/me/project"),
                Path::new("/users/me"),
                Path::new("/users/me/.local/share/zmux"),
                |_| true,
                |_| Err(std::io::Error::from(std::io::ErrorKind::NotFound)),
            ),
            Some(IndexRootBlockReason::Unverifiable)
        );
    }

    #[cfg(unix)]
    #[test]
    fn path_identity_matches_aliases_without_rewriting_logical_paths() {
        let base =
            std::env::temp_dir().join(format!("zmux-path-identity-{}", uuid::Uuid::new_v4()));
        let physical = base.join("physical");
        let logical = base.join("logical");
        std::fs::create_dir_all(&physical).unwrap();
        std::os::unix::fs::symlink(&physical, &logical).unwrap();

        let cache = path_cache();
        assert!(
            !paths_match(&cache, &logical, &physical),
            "foreground comparisons must not resolve uncached filesystem aliases"
        );
        assert_eq!(
            probe_canonical_identity(&cache, &logical),
            Some(physical.clone())
        );
        assert!(paths_match(&cache, &logical, &physical));
        assert_ne!(logical, physical);

        std::fs::remove_dir_all(base).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn path_identity_matches_macos_var_alias() {
        let logical = PathBuf::from("/var");
        let physical = PathBuf::from("/private/var");
        let cache = path_cache();
        assert!(probe_paths_match(&cache, &logical, &physical));
        assert!(paths_match(&cache, &logical, &physical));
        assert_eq!(logical, PathBuf::from("/var"));
    }

    #[cfg(windows)]
    #[test]
    fn path_identity_matches_windows_verbatim_canonical_form() {
        let logical = std::env::temp_dir();
        let canonical = logical.canonicalize().unwrap();
        let cache = path_cache();
        assert!(probe_paths_match(&cache, &logical, &canonical));
        assert!(paths_match(&cache, &logical, &canonical));
        assert_eq!(logical, std::env::temp_dir());
    }

    #[test]
    fn path_context_cache_reuses_filesystem_lookups() {
        use std::cell::Cell;

        let mut cache = PathContextCache::default();
        let canonical_calls = Cell::new(0);
        let path = Path::new("/logical/repository");
        let expected = PathBuf::from("/physical/repository");
        assert_eq!(
            cache.canonical_identity_with(path, |_| {
                canonical_calls.set(canonical_calls.get() + 1);
                Ok(expected.clone())
            }),
            Some(expected.clone())
        );
        assert_eq!(
            cache.canonical_identity_with(path, |_| {
                canonical_calls.set(canonical_calls.get() + 1);
                Ok(PathBuf::from("/unexpected"))
            }),
            Some(expected)
        );
        assert_eq!(canonical_calls.get(), 1);

        let git_calls = Cell::new(0);
        let directory = Path::new("/repo/src/bin");
        let first = cache.nearest_git_root_with(directory, |candidate| {
            git_calls.set(git_calls.get() + 1);
            candidate == Path::new("/repo")
        });
        let calls_after_first = git_calls.get();
        let second = cache.nearest_git_root_with(directory, |_| {
            git_calls.set(git_calls.get() + 1);
            false
        });
        assert_eq!(first, Some(PathBuf::from("/repo")));
        assert_eq!(second, first);
        assert_eq!(git_calls.get(), calls_after_first);
    }

    #[test]
    fn path_context_cache_evicts_only_the_lru_entry_at_capacity() {
        let mut cache = PathContextCache::default();
        for index in 0..PATH_CONTEXT_CACHE_CAPACITY {
            cache.canonical_identity_with(Path::new(&format!("/path-{index}")), |_| {
                Err(std::io::Error::from(std::io::ErrorKind::NotFound))
            });
            cache.nearest_git_root_with(Path::new(&format!("/repo-{index}")), |_| false);
        }
        assert_eq!(cache.canonical_paths.len(), PATH_CONTEXT_CACHE_CAPACITY);
        assert_eq!(cache.git_roots.len(), PATH_CONTEXT_CACHE_CAPACITY);

        // Refresh the oldest entries so the next-oldest entries are selected
        // for eviction rather than dropping the still-hot cache wholesale.
        cache.canonical_identity_with(Path::new("/path-0"), |_| {
            panic!("a retained canonical entry must not be resolved again")
        });
        cache.nearest_git_root_with(Path::new("/repo-0"), |_| {
            panic!("a retained Git-root entry must not be probed again")
        });
        cache.canonical_identity_with(Path::new("/overflow"), |_| {
            Err(std::io::Error::from(std::io::ErrorKind::NotFound))
        });
        cache.nearest_git_root_with(Path::new("/repo-overflow"), |_| false);
        assert_eq!(cache.canonical_paths.len(), PATH_CONTEXT_CACHE_CAPACITY);
        assert_eq!(cache.git_roots.len(), PATH_CONTEXT_CACHE_CAPACITY);
        assert!(cache.canonical_paths.contains_key(Path::new("/path-0")));
        assert!(!cache.canonical_paths.contains_key(Path::new("/path-1")));
        assert!(cache.canonical_paths.contains_key(Path::new("/overflow")));
        assert!(cache.git_roots.contains_key(Path::new("/repo-0")));
        assert!(!cache.git_roots.contains_key(Path::new("/repo-1")));
        assert!(cache.git_roots.contains_key(Path::new("/repo-overflow")));
    }

    #[test]
    fn git_root_cache_invalidation_refreshes_positive_entries() {
        let mut cache = PathContextCache::default();
        let directory = Path::new("/repo/src");
        assert_eq!(
            cache.nearest_git_root_with(directory, |candidate| candidate == Path::new("/repo")),
            Some(PathBuf::from("/repo"))
        );
        assert_eq!(
            cache.git_root_recheck(directory, Duration::from_secs(1)),
            GitRootRecheck::Positive
        );
        cache.clear();
        assert_eq!(cache.nearest_git_root_with(directory, |_| false), None);
        cache.clear();
        assert_eq!(
            cache.git_root_recheck(directory, Duration::from_secs(1)),
            GitRootRecheck::Missing
        );
    }

    #[test]
    fn root_scoped_invalidation_preserves_unrelated_cache_entries() {
        let mut cache = PathContextCache::default();
        let root_a = Path::new("/repos/a");
        let root_b = Path::new("/repos/b");
        cache.canonical_paths.insert(
            root_a.join("src"),
            CanonicalPathCacheEntry {
                identity: Some(PathBuf::from("/physical/a/src")),
                last_access: 1,
            },
        );
        cache.canonical_paths.insert(
            root_b.join("src"),
            CanonicalPathCacheEntry {
                identity: Some(PathBuf::from("/physical/b/src")),
                last_access: 2,
            },
        );
        cache.git_roots.insert(
            root_a.join("src"),
            GitRootCacheEntry {
                root: Some(root_a.to_path_buf()),
                checked_at: Instant::now(),
                last_access: 3,
            },
        );
        cache.git_roots.insert(
            root_b.join("src"),
            GitRootCacheEntry {
                root: Some(root_b.to_path_buf()),
                checked_at: Instant::now(),
                last_access: 4,
            },
        );

        assert_eq!(cache.invalidate_root(root_a), vec![root_a.join("src")]);
        assert!(!cache.canonical_paths.contains_key(&root_a.join("src")));
        assert!(!cache.git_roots.contains_key(&root_a.join("src")));
        assert!(cache.canonical_paths.contains_key(&root_b.join("src")));
        assert!(cache.git_roots.contains_key(&root_b.join("src")));
    }

    #[test]
    fn negative_git_root_rechecks_retain_per_cwd_deadlines() {
        let mut cache = PathContextCache::default();
        let due = PathBuf::from("/outside/due");
        let pending = PathBuf::from("/outside/pending");
        cache.git_roots.insert(
            due.clone(),
            GitRootCacheEntry {
                root: None,
                checked_at: Instant::now() - Duration::from_secs(2),
                last_access: 1,
            },
        );
        cache.git_roots.insert(
            pending.clone(),
            GitRootCacheEntry {
                root: None,
                checked_at: Instant::now() - Duration::from_millis(100),
                last_access: 2,
            },
        );

        assert_eq!(
            cache.git_root_recheck(&due, Duration::from_secs(1)),
            GitRootRecheck::Due
        );
        let GitRootRecheck::Pending(delay) =
            cache.git_root_recheck(&pending, Duration::from_secs(1))
        else {
            panic!("the newer negative entry must keep its own deadline");
        };
        assert!(delay > Duration::from_millis(800));
        assert!(!cache.git_roots.contains_key(&due));
        assert!(cache.git_roots.contains_key(&pending));
    }

    #[test]
    fn terminal_wakeup_policy_discovers_git_init_after_negative_cache() {
        let base =
            std::env::temp_dir().join(format!("zmux-git-init-cache-{}", uuid::Uuid::new_v4()));
        let directory = base.join("src");
        std::fs::create_dir_all(&directory).unwrap();
        let mut cache = PathContextCache::default();
        assert_eq!(
            cache.nearest_git_root_with(&directory, |candidate| candidate.join(".git").exists()),
            None
        );

        std::fs::create_dir_all(base.join(".git")).unwrap();
        assert_eq!(
            cache.nearest_git_root_with(&directory, |candidate| candidate.join(".git").exists()),
            None
        );
        assert_eq!(
            cache.git_root_recheck(&directory, Duration::ZERO),
            GitRootRecheck::Due
        );
        assert_eq!(
            cache.nearest_git_root_with(&directory, |candidate| candidate.join(".git").exists()),
            Some(base.clone())
        );

        std::fs::remove_dir_all(base).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn alias_roots_are_deduplicated_for_reconciliation_and_open_sets() {
        let base = std::env::temp_dir().join(format!("zmux-alias-roots-{}", uuid::Uuid::new_v4()));
        let physical = base.join("physical");
        let logical = base.join("logical");
        std::fs::create_dir_all(&physical).unwrap();
        std::os::unix::fs::symlink(&physical, &logical).unwrap();
        let cache = path_cache();
        assert!(probe_paths_match(&cache, &logical, &physical));

        let counts = git_root_reference_counts([[physical.clone(), logical.clone()].as_slice()]);
        let reconciliation = plan_git_root_reconciliation(
            counts,
            &BTreeSet::from([physical.clone(), logical.clone()]),
            &BTreeSet::new(),
            &cache,
        );
        assert!(reconciliation.added.is_empty());
        assert_eq!(reconciliation.removed.len(), 1);

        let mut open = Vec::new();
        push_unique_logical_path(&mut open, logical.clone(), &cache);
        push_unique_logical_path(&mut open, physical.clone(), &cache);
        assert_eq!(open, vec![logical.clone()]);
        assert_eq!(
            matched_logical_git_root(&open, &physical, &cache),
            Some(logical.clone())
        );

        let mut selected = Some(physical.clone());
        reconcile_selected_git_root(
            &mut selected,
            None,
            std::slice::from_ref(&logical),
            GitDiscoveryState::Authoritative,
            &cache,
        );
        assert_eq!(selected, Some(logical));

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

        let context = finalize_workspace_context(
            WorkspaceContext {
                working_directories: vec![repo_a.join("src"), outside, repo_b.clone()],
                shell_count: 3,
                ..WorkspaceContext::default()
            },
            &path_cache(),
        );

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

        let context = finalize_workspace_context(
            WorkspaceContext {
                working_directories: vec![base.join("api"), base.join("web")],
                shell_count: 2,
                ..WorkspaceContext::default()
            },
            &path_cache(),
        );

        assert_eq!(context.git_roots, vec![base.clone()]);
        assert_eq!(context.git_root, Some(base.clone()));
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn inactive_restored_workspace_keeps_selection_before_discovery() {
        let selected_root = PathBuf::from("/repos/selected");
        let mut selected = Some(selected_root.clone());

        reconcile_selected_git_root(
            &mut selected,
            None,
            &[],
            GitDiscoveryState::Restoring,
            &path_cache(),
        );

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
            None,
            std::slice::from_ref(&first_root),
            GitDiscoveryState::Restoring,
            &path_cache(),
        );
        assert_eq!(selected, Some(selected_root.clone()));
        reconcile_selected_git_root(
            &mut selected,
            None,
            std::slice::from_ref(&first_root),
            GitDiscoveryState::Discovering,
            &path_cache(),
        );
        assert_eq!(selected, Some(selected_root.clone()));

        reconcile_selected_git_root(
            &mut selected,
            None,
            &[first_root, selected_root.clone()],
            GitDiscoveryState::Authoritative,
            &path_cache(),
        );
        assert_eq!(selected, Some(selected_root));
    }

    #[test]
    fn authoritative_discovery_replaces_a_missing_selection() {
        let discovered_root = PathBuf::from("/repos/current");
        let mut selected = Some(PathBuf::from("/repos/gone"));

        reconcile_selected_git_root(
            &mut selected,
            None,
            std::slice::from_ref(&discovered_root),
            GitDiscoveryState::Authoritative,
            &path_cache(),
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
    fn explicit_promotions_are_bounded_and_update_the_pin() {
        let cache = path_cache();
        let mut selected = None;
        let mut pinned = None;
        let mut promoted = VecDeque::new();
        for index in 0..8 {
            promote_git_root(
                &mut selected,
                &mut pinned,
                &mut promoted,
                PathBuf::from(format!("/repos/{index}")),
                &cache,
            );
        }
        assert_eq!(selected, Some(PathBuf::from("/repos/7")));
        assert_eq!(pinned, selected);
        assert_eq!(promoted.len(), 3);
        assert_eq!(
            promoted,
            VecDeque::from([
                PathBuf::from("/repos/7"),
                PathBuf::from("/repos/6"),
                PathBuf::from("/repos/5"),
            ])
        );
    }

    #[test]
    fn metadata_probe_results_require_matching_root_and_generation() {
        let root = Path::new("/repos/current");
        assert!(metadata_probe_is_current(Some(root), 7, root, 7));
        assert!(!metadata_probe_is_current(
            Some(Path::new("/repos/new")),
            7,
            root,
            7
        ));
        assert!(!metadata_probe_is_current(Some(root), 8, root, 7));
    }

    #[test]
    fn stale_workspace_context_probe_cannot_overwrite_a_newer_result() {
        let stale = WorkspaceContext {
            working_directories: vec![PathBuf::from("/repos/old")],
            git_roots: vec![PathBuf::from("/repos/old")],
            git_root: Some(PathBuf::from("/repos/old")),
            ..WorkspaceContext::default()
        };
        let newest = WorkspaceContext {
            working_directories: vec![PathBuf::from("/repos/new")],
            git_roots: vec![PathBuf::from("/repos/new")],
            git_root: Some(PathBuf::from("/repos/new")),
            ..WorkspaceContext::default()
        };
        let current_generation = Some(2);
        let mut applied = WorkspaceContext::default();

        // Complete generation 2 first, then deliver generation 1 late. This is
        // the order produced when an older filesystem probe stalls.
        for (generation, observed) in [(2, newest.clone()), (1, stale)] {
            if let Some(observed) =
                accept_workspace_context_probe_result(current_generation, generation, observed)
            {
                applied = observed;
            }
        }

        assert_eq!(applied, newest);
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

        let reconciliation =
            plan_git_root_reconciliation(counts, &attached, &pending, &path_cache());

        assert_eq!(reconciliation.added, BTreeSet::from([first]));
        assert_eq!(reconciliation.removed, BTreeSet::from([stale]));
    }

    #[test]
    fn git_root_reconciliation_stress_returns_to_active_roots() {
        let visited = (0..256)
            .map(|index| PathBuf::from(format!("/repos/visited-{index}")))
            .collect::<BTreeSet<_>>();
        let active = visited.iter().rev().take(3).cloned().collect::<Vec<_>>();
        let reference_counts = git_root_reference_counts([active.as_slice()]);

        let reconciliation = plan_git_root_reconciliation(
            reference_counts,
            &visited,
            &BTreeSet::new(),
            &path_cache(),
        );
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
            physical_temp_dir().join(format!("zmux-multi-repo-restore-{}", uuid::Uuid::new_v4()));
        let first = base.join("first");
        let selected = base.join("selected");
        std::fs::create_dir_all(first.join(".git")).unwrap();
        std::fs::create_dir_all(selected.join(".git")).unwrap();

        initialize_zmux(cx).await;
        let open = cx.update(|cx| crate::app::open_zmux_workspace_at(None, base.clone(), cx));
        let opened = open.await.expect("workspace should open");
        let panel = opened.workspace.read_with(cx, |workspace, cx| {
            workspace
                .panel::<WorkspacesPanel>(cx)
                .expect("workspaces panel should be installed")
        });
        panel.update(cx, |panel, cx| {
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
                pinned_git_root: None,
                promoted_git_roots: VecDeque::new(),
                git_discovery: GitDiscoveryState::Restoring,
                git: MetadataState::NotRequested,
                metadata_root: None,
                metadata_generation: 0,
                metadata_checked_at: None,
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

        initialize_zmux(cx).await;
        let open = cx.update(|cx| crate::app::open_zmux_workspace_at(None, base.clone(), cx));
        let opened = open.await.expect("workspace should open");
        let panel = opened.workspace.read_with(cx, |workspace, cx| {
            workspace.panel::<WorkspacesPanel>(cx).unwrap()
        });
        panel.update(cx, |panel, _| {
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
                pinned_git_root: None,
                promoted_git_roots: VecDeque::new(),
                git_discovery: GitDiscoveryState::Restoring,
                git: MetadataState::NotRequested,
                metadata_root: None,
                metadata_generation: 0,
                metadata_checked_at: None,
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

        for _ in 0..200 {
            cx.run_until_parked();
            if panel.read_with(cx, |panel, _| {
                panel
                    .entries
                    .iter()
                    .find(|entry| entry.id == 2)
                    .is_some_and(|entry| {
                        entry.git_discovery == GitDiscoveryState::Authoritative
                            && entry.selected_git_root.is_none()
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
            physical_temp_dir().join(format!("zmux-interrupted-restore-{}", uuid::Uuid::new_v4()));
        let selected = base.join("selected");
        std::fs::create_dir_all(selected.join(".git")).unwrap();

        initialize_zmux(cx).await;
        let open = cx.update(|cx| crate::app::open_zmux_workspace_at(None, base.clone(), cx));
        let opened = open.await.expect("workspace should open");
        let panel = opened.workspace.read_with(cx, |workspace, cx| {
            workspace.panel::<WorkspacesPanel>(cx).unwrap()
        });
        panel.update(cx, |panel, _| {
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
                pinned_git_root: None,
                promoted_git_roots: VecDeque::new(),
                git_discovery: GitDiscoveryState::Restoring,
                git: MetadataState::NotRequested,
                metadata_root: None,
                metadata_generation: 0,
                metadata_checked_at: None,
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
        std::fs::create_dir_all(first.join(".git")).unwrap();
        std::fs::create_dir_all(second.join(".git")).unwrap();

        initialize_zmux(cx).await;
        let open = cx.update(|cx| crate::app::open_zmux_workspace_at(None, base.clone(), cx));
        let opened = open.await.expect("workspace should open");
        let panel = opened.workspace.read_with(cx, |workspace, cx| {
            workspace.panel::<WorkspacesPanel>(cx).unwrap()
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

        for _ in 0..200 {
            cx.run_until_parked();
            if panel.read_with(cx, |panel, _| {
                panel.open_git_roots() == vec![second.clone(), first.clone()]
            }) {
                break;
            }
            cx.background_executor
                .timer(Duration::from_millis(10))
                .await;
        }

        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.entries.len(), 2, "an open worktree was duplicated");
            assert_eq!(panel.active, worktree_id);
            assert_eq!(panel.open_git_roots(), vec![second.clone(), first.clone()]);
        });

        let _ = std::fs::remove_dir_all(base);
    }

    #[gpui::test]
    async fn directory_navigation_creates_a_logical_workspace_without_a_worktree(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.executor().allow_parking();
        let base = std::env::temp_dir().join(format!(
            "zmux-directory-navigation-{}",
            uuid::Uuid::new_v4()
        ));
        let directory = base.join("nested");
        std::fs::create_dir_all(&directory).unwrap();

        initialize_zmux(cx).await;
        let open = cx.update(|cx| crate::app::open_zmux_workspace_at(None, base.clone(), cx));
        let opened = open.await.expect("workspace should open");
        let panel = opened.workspace.read_with(cx, |workspace, cx| {
            workspace.panel::<WorkspacesPanel>(cx).unwrap()
        });
        opened
            .window
            .update(cx, |_, window, cx| {
                panel.update(cx, |panel, cx| {
                    panel.open_directory_workspace(directory.clone(), window, cx);
                    panel.open_directory_workspace(directory.clone(), window, cx);
                });
            })
            .unwrap();

        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.entries.len(), 2, "the same directory was duplicated");
            let active = panel
                .entries
                .iter()
                .find(|entry| entry.id == panel.active)
                .unwrap();
            assert_eq!(
                active.default_directory.as_deref(),
                Some(directory.as_path())
            );
        });
        let worktree_count = opened.workspace.read_with(cx, |workspace, cx| {
            workspace.project().read(cx).worktrees(cx).count()
        });
        assert_eq!(worktree_count, 0);
        let _ = std::fs::remove_dir_all(base);
    }

    #[gpui::test]
    async fn visited_repository_uses_probe_until_explicitly_pinned(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let base =
            std::env::temp_dir().join(format!("zmux-probed-repository-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&base).unwrap();
        let init = std::process::Command::new("git")
            .arg("-C")
            .arg(&base)
            .args(["init", "-q"])
            .status()
            .unwrap();
        assert!(init.success());

        initialize_zmux(cx).await;
        let open = cx.update(|cx| crate::app::open_zmux_workspace_at(None, base.clone(), cx));
        let opened = open.await.expect("workspace should open");
        let panel = opened.workspace.read_with(cx, |workspace, cx| {
            workspace.panel::<WorkspacesPanel>(cx).unwrap()
        });

        for _ in 0..200 {
            cx.run_until_parked();
            if panel.read_with(cx, |panel, _| {
                panel.entries.iter().any(|entry| {
                    entry.selected_git_root.as_deref() == Some(base.as_path())
                        && matches!(entry.git, MetadataState::Ready(_))
                })
            }) {
                break;
            }
            cx.background_executor
                .timer(Duration::from_millis(10))
                .await;
        }
        assert!(panel.read_with(cx, |panel, _| {
            panel.entries.iter().any(|entry| {
                entry.selected_git_root.as_deref() == Some(base.as_path())
                    && matches!(entry.git, MetadataState::Ready(_))
            })
        }));
        assert_eq!(
            opened.workspace.read_with(cx, |workspace, cx| {
                workspace.project().read(cx).worktrees(cx).count()
            }),
            0,
            "visiting a repository must not attach a recursively watched worktree"
        );

        panel.update(cx, |panel, cx| {
            panel.pin_active_git_root(base.clone(), cx);
        });
        for _ in 0..200 {
            cx.run_until_parked();
            if panel.read_with(cx, |panel, _| panel.attached_worktrees.len()) == 1 {
                break;
            }
            cx.background_executor
                .timer(Duration::from_millis(10))
                .await;
        }
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.attached_worktrees.len()),
            1,
            "an explicit pin should enable Zed's full Git integration"
        );

        let _ = std::fs::remove_dir_all(base);
    }

    #[gpui::test]
    async fn project_event_audit_removes_an_unadmitted_directory_worktree(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.executor().allow_parking();
        let base =
            std::env::temp_dir().join(format!("zmux-unadmitted-worktree-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&base).unwrap();

        initialize_zmux(cx).await;
        let open = cx.update(|cx| crate::app::open_zmux_workspace_at(None, base.clone(), cx));
        let opened = open.await.expect("workspace should open");
        cx.run_until_parked();

        let project = opened
            .workspace
            .read_with(cx, |workspace, _| workspace.project().clone());
        let create = project.update(cx, |project, cx| project.create_worktree(&base, true, cx));
        let inadmissible = create.await.expect("test worktree should be created");
        let inadmissible_id = inadmissible.read_with(cx, |worktree, _| worktree.id());

        for _ in 0..100 {
            cx.run_until_parked();
            if project.read_with(cx, |project, cx| {
                project.worktree_for_id(inadmissible_id, cx).is_none()
            }) {
                break;
            }
            cx.background_executor
                .timer(Duration::from_millis(10))
                .await;
        }

        assert!(
            project.read_with(cx, |project, cx| {
                project.worktree_for_id(inadmissible_id, cx).is_none()
            }),
            "a non-Git directory added through a bypass path must be detached"
        );
        let panel = opened.workspace.read_with(cx, |workspace, cx| {
            workspace.panel::<WorkspacesPanel>(cx).unwrap()
        });
        panel.update(cx, |panel, cx| panel.reconcile_git_context(cx));
        assert!(panel.read_with(cx, |panel, _| {
            panel.audited_blocked_roots.contains(&base) && panel.warned_scan_roots.contains(&base)
        }));
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

        initialize_zmux(cx).await;
        let open = cx.update(|cx| crate::app::open_zmux_workspace_at(None, shared.clone(), cx));
        let opened = open.await.expect("workspace should open");
        let panel = opened.workspace.read_with(cx, |workspace, cx| {
            workspace
                .panel::<WorkspacesPanel>(cx)
                .expect("workspaces panel should be installed")
        });
        warm_git_context_cache(&panel, vec![shared.clone(), unique.clone()], cx).await;
        panel.update(cx, |panel, cx| {
            let active = panel
                .entries
                .iter_mut()
                .find(|entry| entry.id == panel.active)
                .unwrap();
            active.context.git_roots = vec![shared.clone()];
            active.selected_git_root = Some(shared.clone());
            active.pinned_git_root = Some(shared.clone());
            active.promoted_git_roots = VecDeque::from([shared.clone()]);
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
                pinned_git_root: Some(unique.clone()),
                promoted_git_roots: VecDeque::from([unique.clone()]),
                git_discovery: GitDiscoveryState::Authoritative,
                git: MetadataState::NotRequested,
                metadata_root: None,
                metadata_generation: 0,
                metadata_checked_at: None,
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
                panel.update(cx, |panel, cx| {
                    // Terminal context refreshes run asynchronously and may
                    // have marked the active entry incomplete while the two
                    // worktrees were attaching. Establish this test's
                    // authoritative-root precondition in the same update as
                    // the close so cleanup is not timing-dependent.
                    let active = panel
                        .entries
                        .iter_mut()
                        .find(|entry| entry.id == panel.active)
                        .unwrap();
                    active.context.git_roots = vec![shared.clone()];
                    active.context_authoritative = true;
                    active.git_discovery = GitDiscoveryState::Authoritative;
                    panel.close_workspace(2, window, cx);
                });
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

    #[gpui::test]
    async fn closing_workspace_purges_parked_terminal_bookkeeping(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let base = std::env::temp_dir().join(format!(
            "zmux-close-workspace-bookkeeping-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&base).unwrap();

        initialize_zmux(cx).await;
        let open = cx.update(|cx| crate::app::open_zmux_workspace_at(None, base.clone(), cx));
        let opened = open.await.expect("workspace should open");
        let panel = opened.workspace.read_with(cx, |workspace, cx| {
            workspace
                .panel::<WorkspacesPanel>(cx)
                .expect("workspaces panel should be installed")
        });
        let origin = panel.read_with(cx, |panel, _| panel.active_workspace_id());

        opened
            .window
            .update(cx, |_, window, cx| {
                panel.update(cx, |panel, cx| panel.create_workspace(window, cx));
            })
            .expect("window should remain open");
        let closing = panel.read_with(cx, |panel, _| panel.active_workspace_id());
        assert_ne!(closing, origin);

        for _ in 0..100 {
            cx.run_until_parked();
            if panel.read_with(cx, |panel, _| {
                panel
                    .terminal_registry
                    .values()
                    .any(|terminal| terminal.workspace_id == closing)
            }) {
                break;
            }
            cx.background_executor
                .timer(Duration::from_millis(10))
                .await;
        }

        let closing_items = opened
            .window
            .update(cx, |_, window, cx| {
                panel.update(cx, |panel, cx| {
                    let item_ids = panel
                        .terminal_registry
                        .iter()
                        .filter_map(|(item_id, terminal)| {
                            (terminal.workspace_id == closing).then_some(*item_id)
                        })
                        .collect::<HashSet<_>>();
                    assert!(
                        !item_ids.is_empty(),
                        "closing workspace needs a live terminal"
                    );
                    assert_eq!(panel.active_workspace_id(), closing);
                    for item_id in &item_ids {
                        panel.dirty_agent_terminals.insert(*item_id);
                        panel.agent_refresh_queue.schedule(*item_id, Instant::now());
                    }
                    // Closing the active workspace parks its layout while
                    // activating the fallback, then immediately drops that
                    // parked entry. This is the path whose ItemRemoved events
                    // cannot clean the per-terminal registries for us.
                    panel.close_workspace(closing, window, cx);
                    item_ids
                })
            })
            .expect("window should remain open");

        panel.read_with(cx, |panel, _| {
            assert!(
                panel
                    .terminal_registry
                    .values()
                    .all(|terminal| { terminal.workspace_id != closing })
            );
            assert!(
                closing_items
                    .iter()
                    .all(|item_id| !panel.dirty_agent_terminals.contains(item_id))
            );
            assert!(
                closing_items
                    .iter()
                    .all(|item_id| { !panel.agent_refresh_queue.deadlines.contains_key(item_id) })
            );
        });

        let _ = std::fs::remove_dir_all(base);
    }
}
