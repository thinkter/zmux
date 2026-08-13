//! Opening zmux windows: builds the Zed [`Workspace`], installs the
//! [`WorkspacesPanel`] and Git panel, registers zmux actions, and either
//! restores the persisted startup layout or spawns the first terminal.

use std::path::PathBuf;

use gpui::{App, AppContext, Task, TaskExt, WindowHandle};
use workspace::{AppState, MultiWorkspace, OpenMode, OpenResult, Workspace};

use crate::env::terminal_env;
use crate::notification_runtime::NotificationRuntime;
use crate::welcome::ZmuxWelcome;
use crate::workspaces::{WorkspacesPanel, register_git_repository_scope, restore_startup_layout};

use super::actions::register_zmux_actions;
use super::terminal::{create_center_terminal, create_restored_terminals_for_workspace};

pub fn open_zmux_workspace(
    requesting_window: Option<WindowHandle<MultiWorkspace>>,
    cx: &mut App,
) -> Task<anyhow::Result<OpenResult>> {
    open_zmux_workspace_for_directory(
        requesting_window,
        crate::env::current_working_directory(),
        true,
        cx,
    )
}

/// Open a zmux window rooted at an explicit directory.
///
/// Besides being useful for embedders, this keeps tests independent from a
/// persisted Zed workspace for the repository that happens to run them.
pub fn open_zmux_workspace_at(
    requesting_window: Option<WindowHandle<MultiWorkspace>>,
    initial_dir: PathBuf,
    cx: &mut App,
) -> Task<anyhow::Result<OpenResult>> {
    open_zmux_workspace_for_directory(requesting_window, Some(initial_dir), false, cx)
}

fn open_zmux_workspace_for_directory(
    requesting_window: Option<WindowHandle<MultiWorkspace>>,
    initial_directory: Option<PathBuf>,
    restore_persisted_session: bool,
    cx: &mut App,
) -> Task<anyhow::Result<OpenResult>> {
    let app_state = AppState::global(cx);

    // A terminal's working directory is not a Zed project root. In particular,
    // desktop launchers commonly start zmux in the user's home directory; using
    // that path here causes Zed's worktree scanner to recursively index and
    // watch the entire home tree. Start with a pathless project and let
    // WorkspacesPanel attach only exact, admitted Git roots discovered from
    // live terminal directories.
    let open = Workspace::new_local(
        Vec::new(),
        app_state,
        requesting_window,
        // Route capabilities are per terminal and must never enter the
        // project-wide environment inherited by every shell.
        Some(terminal_env()),
        Some(Box::new(move |workspace, window, cx| {
            crate::visual_power::VisualPowerMonitor::attach(window, cx);
            let welcome = cx.new(ZmuxWelcome::new);
            let center_pane = workspace.active_pane().clone();
            center_pane.update(cx, |pane, cx| {
                pane.set_should_display_welcome_page(false);
                pane.add_item(Box::new(welcome), true, true, None, window, cx);
            });
            workspace
                .bottom_dock()
                .update(cx, |dock, cx| dock.set_open(false, window, cx));

            let panel = cx.new(|cx| {
                WorkspacesPanel::new(
                    workspace.weak_handle(),
                    initial_directory,
                    restore_persisted_session,
                    window,
                    cx,
                )
            });
            workspace.add_panel(panel.clone(), window, cx);
            register_git_repository_scope(workspace.project(), &panel, cx);
            workspace.open_panel::<WorkspacesPanel>(window, cx);
            cx.spawn_in(window, async move |workspace_handle, cx| {
                let project_panel =
                    project_panel::ProjectPanel::load(workspace_handle.clone(), cx.clone()).await?;
                let git_panel =
                    git_ui::git_panel::GitPanel::load(workspace_handle.clone(), cx.clone()).await?;
                workspace_handle.update_in(cx, |workspace, window, cx| {
                    workspace.add_panel(project_panel, window, cx);
                    workspace.add_panel(git_panel, window, cx);
                    // The explorer owns the right dock initially. Git remains
                    // its adjacent dock tab; the workspace switcher stays open
                    // in the independent left dock.
                    workspace.open_panel::<project_panel::ProjectPanel>(window, cx);
                    workspace.open_panel::<WorkspacesPanel>(window, cx);
                })?;
                anyhow::Ok(())
            })
            .detach_and_log_err(cx);
            NotificationRuntime::attach_workspace(cx.entity(), panel.clone(), window, cx);

            register_zmux_actions(workspace);

            let startup_restore = panel.read(cx).initial_restore();
            if let Some(layout) = startup_restore {
                let workspace_id = panel.read(cx).active_workspace_id();
                let generation = panel.read(cx).active_workspace_generation();
                let terminals = restore_startup_layout(workspace, &layout, window, cx);
                create_restored_terminals_for_workspace(
                    workspace,
                    workspace_id,
                    generation,
                    terminals,
                    window,
                    cx,
                )
                .detach_and_log_err(cx);
            } else {
                create_center_terminal(workspace, window, cx).detach_and_log_err(cx);
            }
        })),
        OpenMode::NewWindow,
        cx,
    );
    cx.spawn(async move |cx| {
        let result = open.await?;
        let workspace = result.workspace.clone();
        result.window.update(cx, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                // `new_local` restores Zed's persisted dock visibility after
                // the initializer runs. Reassert Zmux's primary panel once
                // that lifecycle is complete, especially for pathless
                // projects where startup finishes immediately.
                workspace.open_panel::<WorkspacesPanel>(window, cx);
            });
        })?;
        anyhow::Ok(result)
    })
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use gpui::{Action, TestAppContext};
    use terminal_view::TerminalView;
    use workspace::{OpenResult, Workspace};

    use crate::session::{CrashFlushOutcome, LayoutNodeSnapshot, SessionSnapshot, SessionStore};
    use crate::workspaces::install_session_store_for_test;
    use crate::{SplitTerminalRight, init_zmux};

    use super::open_zmux_workspace_for_directory;

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "zmux-{name}-{}-{}",
                std::process::id(),
                uuid::Uuid::new_v4()
            ));
            std::fs::create_dir_all(&path).expect("create isolated session test directory");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn terminal_count(workspace: &Workspace, cx: &gpui::App) -> usize {
        workspace
            .panes()
            .iter()
            .map(|pane| {
                pane.read(cx)
                    .items()
                    .filter(|item| item.act_as::<TerminalView>(cx).is_some())
                    .count()
            })
            .sum()
    }

    fn snapshot_terminal_count(snapshot: &SessionSnapshot) -> usize {
        fn layout_terminal_count(node: &LayoutNodeSnapshot) -> usize {
            match node {
                LayoutNodeSnapshot::Leaf { tabs, .. } => tabs.len(),
                LayoutNodeSnapshot::Split { first, second, .. } => {
                    layout_terminal_count(first) + layout_terminal_count(second)
                }
            }
        }

        snapshot
            .workspaces
            .iter()
            .map(|workspace| layout_terminal_count(&workspace.layout.root))
            .sum()
    }

    async fn wait_for_terminal_count(
        opened: &OpenResult,
        expected: usize,
        cx: &mut TestAppContext,
    ) {
        for _ in 0..100 {
            cx.run_until_parked();
            if opened.workspace.read_with(cx, terminal_count) == expected {
                return;
            }
            cx.background_executor
                .timer(Duration::from_millis(20))
                .await;
        }
        assert_eq!(opened.workspace.read_with(cx, terminal_count), expected);
    }

    async fn wait_for_persisted_terminal_count(
        store: &SessionStore,
        expected: usize,
        cx: &mut TestAppContext,
    ) -> SessionSnapshot {
        for _ in 0..100 {
            cx.run_until_parked();
            if let Some(snapshot) = store.load().expect("load test session")
                && snapshot_terminal_count(&snapshot) == expected
            {
                return snapshot;
            }
            cx.background_executor
                .timer(Duration::from_millis(20))
                .await;
        }
        let snapshot = store
            .load()
            .expect("load test session")
            .expect("session should have been persisted");
        assert_eq!(snapshot_terminal_count(&snapshot), expected);
        snapshot
    }

    fn close_window(opened: OpenResult, cx: &mut TestAppContext) {
        let window = opened.window;
        drop(opened);
        window
            .update(cx, |_, window, _cx| window.remove_window())
            .expect("test window should remain open until removal");
        cx.run_until_parked();
    }

    async fn initialize_zmux(cx: &mut TestAppContext) {
        let initialization = cx.update(init_zmux);
        initialization.await;
    }

    #[gpui::test]
    async fn survivor_adopts_session_persistence_and_restores_its_latest_layout(
        cx: &mut TestAppContext,
    ) {
        cx.executor().allow_parking();
        let state = TestDirectory::new("session-handoff-state");
        let primary_directory = TestDirectory::new("session-handoff-primary");
        let transient_directory = TestDirectory::new("session-handoff-transient");
        let survivor_directory = TestDirectory::new("session-handoff-survivor");
        let session_path = state.path().join("session.json");
        let store = SessionStore::at(session_path.clone());

        initialize_zmux(cx).await;
        let primary_task = cx.update(|cx| {
            install_session_store_for_test(store.clone(), cx);
            open_zmux_workspace_for_directory(
                None,
                Some(primary_directory.path().to_path_buf()),
                true,
                cx,
            )
        });
        let primary = primary_task.await.expect("open persistence owner");
        wait_for_terminal_count(&primary, 1, cx).await;
        wait_for_persisted_terminal_count(&store, 1, cx).await;

        let transient_task = cx.update(|cx| {
            open_zmux_workspace_for_directory(
                Some(primary.window),
                Some(transient_directory.path().to_path_buf()),
                false,
                cx,
            )
        });
        let transient = transient_task.await.expect("open non-owner window");
        wait_for_terminal_count(&transient, 1, cx).await;
        close_window(transient, cx);
        let still_primary = store
            .load()
            .expect("load owner session after non-owner close")
            .expect("owner session should remain persisted");
        assert_eq!(
            still_primary.workspaces[0].default_directory.as_deref(),
            Some(primary_directory.path()),
            "closing a non-owner must not transfer persistence"
        );

        let survivor_task = cx.update(|cx| {
            open_zmux_workspace_for_directory(
                Some(primary.window),
                Some(survivor_directory.path().to_path_buf()),
                false,
                cx,
            )
        });
        let survivor = survivor_task.await.expect("open survivor window");
        wait_for_terminal_count(&survivor, 1, cx).await;

        close_window(primary, cx);
        let adopted = wait_for_persisted_terminal_count(&store, 1, cx).await;
        assert_eq!(
            adopted.workspaces[0].default_directory.as_deref(),
            Some(survivor_directory.path()),
            "handoff must snapshot the survivor instead of reloading the old owner"
        );

        survivor
            .window
            .update(cx, |_, window, cx| {
                window.dispatch_action(SplitTerminalRight.boxed_clone(), cx);
            })
            .expect("survivor window should remain open");
        wait_for_terminal_count(&survivor, 2, cx).await;
        let persisted = wait_for_persisted_terminal_count(&store, 2, cx).await;
        assert!(matches!(
            persisted.workspaces[0].layout.root,
            LayoutNodeSnapshot::Split { .. }
        ));

        close_window(survivor, cx);
        let restarted_store = SessionStore::at(session_path);
        let restarted_task = cx.update(|cx| {
            install_session_store_for_test(restarted_store, cx);
            open_zmux_workspace_for_directory(
                None,
                Some(primary_directory.path().to_path_buf()),
                true,
                cx,
            )
        });
        let restarted = restarted_task.await.expect("reopen persisted session");
        wait_for_terminal_count(&restarted, 2, cx).await;
        close_window(restarted, cx);
    }

    #[gpui::test]
    async fn crash_flush_uses_the_layout_captured_before_debounced_io(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let state = TestDirectory::new("panic-session-state");
        let workspace_directory = TestDirectory::new("panic-session-workspace");
        let store = SessionStore::at(state.path().join("session.json"));
        let flusher = crate::session::CrashSessionFlusher::start(store.clone()).unwrap();

        initialize_zmux(cx).await;
        let open_task = cx.update(|cx| {
            install_session_store_for_test(store.clone(), cx);
            open_zmux_workspace_for_directory(
                None,
                Some(workspace_directory.path().to_path_buf()),
                true,
                cx,
            )
        });
        let opened = open_task.await.expect("open persistence owner");
        wait_for_terminal_count(&opened, 1, cx).await;
        wait_for_persisted_terminal_count(&store, 1, cx).await;

        opened
            .window
            .update(cx, |_, window, cx| {
                window.dispatch_action(SplitTerminalRight.boxed_clone(), cx);
            })
            .expect("window should remain open");
        wait_for_terminal_count(&opened, 2, cx).await;

        assert_eq!(
            flusher.flush(Duration::from_secs(1)),
            CrashFlushOutcome::Installed
        );
        let recovered = store
            .load()
            .expect("load crash-flushed session")
            .expect("crash flush should install a session");
        assert_eq!(snapshot_terminal_count(&recovered), 2);

        close_window(opened, cx);
    }

    #[gpui::test]
    async fn standalone_explicit_window_does_not_enable_session_persistence(
        cx: &mut TestAppContext,
    ) {
        cx.executor().allow_parking();
        let state = TestDirectory::new("session-disabled-state");
        let workspace_directory = TestDirectory::new("session-disabled-workspace");
        let store = SessionStore::at(state.path().join("session.json"));

        initialize_zmux(cx).await;
        let open_task = cx.update(|cx| {
            install_session_store_for_test(store.clone(), cx);
            open_zmux_workspace_for_directory(
                None,
                Some(workspace_directory.path().to_path_buf()),
                false,
                cx,
            )
        });
        let opened = open_task.await.expect("open explicit test window");
        wait_for_terminal_count(&opened, 1, cx).await;
        cx.background_executor
            .timer(Duration::from_millis(700))
            .await;
        cx.run_until_parked();
        assert_eq!(store.load().expect("load disabled session store"), None);
        close_window(opened, cx);
    }
}
