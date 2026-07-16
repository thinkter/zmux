//! The agent chat rail's state machine.
//!
//! Observes terminals for known agent CLI processes (sampled every 300ms by
//! [`super::WorkspacesPanel`]'s agent refresh loop), reconciles them into
//! per-workspace [`AgentChat`] rows, and applies hysteresis: settled-state
//! transitions and exited-agent row removal both require
//! `AGENT_STATE_CONFIRMATIONS` consecutive confirming refreshes, so a single
//! transient sampling miss never flaps or tears down a live chat row.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use gpui::EntityId;
use terminal_view::TerminalView;
use ui::prelude::*;
use workspace::Workspace;
use workspace::item::ItemHandle;

use crate::agent_detection::{
    AgentKind, AgentSnapshot, DetectionSignal, detect_agent, sanitized_osc_title, submitted_prompt,
};
use crate::notifications::WorkspaceId;

use super::persistence::StoredLayout;

const AGENT_DETECTION_TAIL_LINES: usize = 80;
const AGENT_STATE_CONFIRMATIONS: u8 = 2;

/// One refresh pass's sample of a single live agent process in a terminal.
#[derive(Clone, Debug, PartialEq, Eq)]
struct AgentObservation {
    kind: AgentKind,
    item_id: EntityId,
    custom_title: Option<String>,
    osc_title: String,
    recent: String,
    cwd: Option<PathBuf>,
}

/// Everything one refresh pass observed about a workspace's terminals.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct AgentWorkspaceObservation {
    agents: Vec<AgentObservation>,
    terminal_item_ids: Vec<EntityId>,
    active_item_id: Option<EntityId>,
}

/// Lifecycle of an agent chat row, from most to least attention-worthy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AgentChatState {
    NeedsInput,
    Working,
    Idle,
    Quiet,
    Open,
}

impl AgentChatState {
    fn label(self, seen: bool) -> &'static str {
        match (self, seen) {
            (Self::NeedsInput, _) => "needs input",
            (Self::Working, _) => "working",
            (Self::Idle, false) => "done",
            (Self::Idle, true) => "idle",
            (Self::Quiet, _) => "quiet",
            (Self::Open, _) => "open",
        }
    }

    pub(super) fn color(self, seen: bool) -> Color {
        match (self, seen) {
            (Self::NeedsInput, _) => Color::Error,
            (Self::Working, _) => Color::Warning,
            (Self::Idle, false) => Color::Accent,
            (Self::Idle, true) => Color::Success,
            (Self::Quiet | Self::Open, _) => Color::Muted,
        }
    }
}

/// A row in the agent chat rail. Identity is `(workspace, terminal item)`,
/// so an agent restarted in the same terminal reuses its row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct AgentChat {
    pub(super) workspace_id: WorkspaceId,
    pub(super) kind: AgentKind,
    pub(super) item_id: EntityId,
    pub(super) custom_title: Option<String>,
    pub(super) osc_title: Option<String>,
    pub(super) prompt_snippet: Option<String>,
    pub(super) cwd: Option<PathBuf>,
    pub(super) state: AgentChatState,
    pub(super) seen: bool,
    pub(super) focused: bool,
    pub(super) had_active_turn: bool,
    pub(super) process_exited: bool,
    pub(super) creation_sequence: u64,
    pub(super) activity_sequence: u64,
    pub(super) missing_refreshes: u8,
    pub(super) pending_state: Option<AgentChatState>,
    pub(super) pending_confirmations: u8,
}

pub(super) fn agent_observation_for_active_workspace(
    workspace: &Workspace,
    cx: &App,
) -> AgentWorkspaceObservation {
    let mut observed = AgentWorkspaceObservation {
        active_item_id: workspace.active_item(cx).map(|item| item.item_id()),
        ..AgentWorkspaceObservation::default()
    };
    for pane in workspace.panes() {
        for item in pane.read(cx).items() {
            add_item_to_agent_observation(item.as_ref(), &mut observed, cx);
        }
    }
    observed
}

pub(super) fn agent_observation_for_stored_layout(
    layout: &StoredLayout,
    cx: &App,
) -> AgentWorkspaceObservation {
    fn visit(layout: &StoredLayout, observed: &mut AgentWorkspaceObservation, cx: &App) {
        match layout {
            StoredLayout::Leaf { items, .. } => {
                for item in items {
                    add_item_to_agent_observation(item.as_ref(), observed, cx);
                }
            }
            StoredLayout::Split { first, second, .. } => {
                visit(first, observed, cx);
                visit(second, observed, cx);
            }
        }
    }

    let mut observed = AgentWorkspaceObservation::default();
    visit(layout, &mut observed, cx);
    observed
}

fn add_item_to_agent_observation(
    item: &dyn ItemHandle,
    observed: &mut AgentWorkspaceObservation,
    cx: &App,
) {
    let Some(terminal_view) = item.act_as::<TerminalView>(cx) else {
        return;
    };
    observed.terminal_item_ids.push(item.item_id());
    let (custom_title, terminal) = {
        let terminal_view = terminal_view.read(cx);
        (
            terminal_view.custom_title().map(str::to_owned),
            terminal_view.terminal().clone(),
        )
    };
    let terminal = terminal.read(cx);
    let Some(process) = terminal.foreground_process_command_name() else {
        return;
    };
    let Some(kind) = AgentKind::from_process(&process) else {
        return;
    };
    let recent = if kind.has_detailed_detection() {
        terminal
            .last_n_non_empty_lines(AGENT_DETECTION_TAIL_LINES)
            .join("\n")
    } else {
        String::new()
    };
    observed.agents.push(AgentObservation {
        kind,
        item_id: item.item_id(),
        custom_title,
        osc_title: terminal.breadcrumb_text.clone(),
        recent,
        cwd: terminal.working_directory(),
    });
}

/// Reconcile one workspace's chat rows against a fresh observation pass;
/// returns whether anything user-visible changed.
pub(super) fn reconcile_agent_chats_for_workspace(
    chats: &mut HashMap<(WorkspaceId, EntityId), AgentChat>,
    next_activity_sequence: &mut u64,
    workspace_id: WorkspaceId,
    observed: &AgentWorkspaceObservation,
) -> bool {
    let live_items = observed
        .terminal_item_ids
        .iter()
        .copied()
        .collect::<HashSet<_>>();
    let running_items = observed
        .agents
        .iter()
        .map(|agent| (agent.item_id, agent))
        .collect::<HashMap<_, _>>();
    let previous_len = chats.len();
    chats.retain(|(chat_workspace_id, item_id), _| {
        *chat_workspace_id != workspace_id || live_items.contains(item_id)
    });
    let mut changed = chats.len() != previous_len;

    for agent in &observed.agents {
        let key = (workspace_id, agent.item_id);
        if let Some(chat) = chats.get_mut(&key) {
            let restarted = chat.process_exited || chat.kind != agent.kind;
            if chat.kind != agent.kind {
                chat.kind = agent.kind;
                chat.prompt_snippet = None;
                changed = true;
            }
            if restarted {
                chat.state = if agent.kind.has_detailed_detection() {
                    AgentChatState::Quiet
                } else {
                    AgentChatState::Open
                };
                chat.seen = true;
                chat.had_active_turn = false;
                chat.process_exited = false;
                chat.pending_state = None;
                chat.pending_confirmations = 0;
                chat.activity_sequence = next_agent_activity_sequence(next_activity_sequence);
                changed = true;
            }
            if chat.custom_title != agent.custom_title {
                chat.custom_title.clone_from(&agent.custom_title);
                changed = true;
            }
            let osc_title = sanitized_osc_title(agent.kind, &agent.osc_title);
            if chat.osc_title != osc_title {
                chat.osc_title = osc_title;
                changed = true;
            }
            if chat.cwd != agent.cwd {
                chat.cwd.clone_from(&agent.cwd);
                changed = true;
            }
            let focused = observed.active_item_id == Some(agent.item_id);
            if chat.focused != focused {
                chat.focused = focused;
                changed = true;
            }
            if focused && chat.state == AgentChatState::Idle && !chat.seen {
                chat.seen = true;
                changed = true;
            }
            if chat.missing_refreshes != 0 {
                chat.missing_refreshes = 0;
                changed = true;
            }
            changed |= apply_agent_observation(chat, agent, next_activity_sequence);
        } else {
            let sequence = next_agent_activity_sequence(next_activity_sequence);
            let outcome = agent.kind.has_detailed_detection().then(|| {
                detect_agent(
                    agent.kind,
                    AgentSnapshot {
                        recent: &agent.recent,
                        osc_title: &agent.osc_title,
                    },
                )
            });
            let state = outcome.map_or(AgentChatState::Open, |outcome| {
                chat_state_for_signal(outcome.signal).unwrap_or(AgentChatState::Quiet)
            });
            chats.insert(
                key,
                AgentChat {
                    workspace_id,
                    kind: agent.kind,
                    item_id: agent.item_id,
                    custom_title: agent.custom_title.clone(),
                    osc_title: sanitized_osc_title(agent.kind, &agent.osc_title),
                    prompt_snippet: (state == AgentChatState::Working)
                        .then(|| submitted_prompt(agent.kind, &agent.recent))
                        .flatten(),
                    cwd: agent.cwd.clone(),
                    state,
                    seen: true,
                    focused: observed.active_item_id == Some(agent.item_id),
                    had_active_turn: matches!(
                        state,
                        AgentChatState::Working | AgentChatState::NeedsInput
                    ),
                    process_exited: false,
                    creation_sequence: sequence,
                    activity_sequence: sequence,
                    missing_refreshes: 0,
                    pending_state: None,
                    pending_confirmations: 0,
                },
            );
            changed = true;
        }
    }

    for chat in chats.values_mut().filter(|chat| {
        chat.workspace_id == workspace_id && !running_items.contains_key(&chat.item_id)
    }) {
        chat.focused = observed.active_item_id == Some(chat.item_id);
        if chat.process_exited {
            if chat.focused && chat.state == AgentChatState::Idle && !chat.seen {
                chat.seen = true;
                changed = true;
            }
            continue;
        }
        let missing_refreshes = chat.missing_refreshes.saturating_add(1);
        if missing_refreshes != chat.missing_refreshes {
            chat.missing_refreshes = missing_refreshes;
            changed = true;
        }
        if chat.missing_refreshes >= AGENT_STATE_CONFIRMATIONS {
            chat.process_exited = true;
            chat.pending_state = None;
            chat.pending_confirmations = 0;
            if chat.state != AgentChatState::Idle {
                chat.state = AgentChatState::Idle;
                chat.seen = chat.focused;
                chat.had_active_turn = false;
                chat.activity_sequence = next_agent_activity_sequence(next_activity_sequence);
            }
            changed = true;
        }
    }

    changed
}

/// Fold a fresh detection outcome into an existing chat, applying the
/// confirmation hysteresis before publishing settled (Idle/Quiet) states.
fn apply_agent_observation(
    chat: &mut AgentChat,
    agent: &AgentObservation,
    next_activity_sequence: &mut u64,
) -> bool {
    if !agent.kind.has_detailed_detection() {
        return publish_chat_state(chat, AgentChatState::Open, next_activity_sequence);
    }

    let outcome = detect_agent(
        agent.kind,
        AgentSnapshot {
            recent: &agent.recent,
            osc_title: &agent.osc_title,
        },
    );
    let Some(next_state) = chat_state_for_signal(outcome.signal) else {
        let changed = chat.pending_state.take().is_some() || chat.pending_confirmations != 0;
        chat.pending_confirmations = 0;
        return changed;
    };

    let mut metadata_changed = false;
    if next_state == AgentChatState::Working
        && let Some(prompt) = submitted_prompt(agent.kind, &agent.recent)
        && chat.prompt_snippet.as_deref() != Some(&prompt)
    {
        chat.prompt_snippet = Some(prompt);
        metadata_changed = true;
    }

    if matches!(
        next_state,
        AgentChatState::Working | AgentChatState::NeedsInput
    ) {
        chat.had_active_turn = true;
        chat.seen = true;
        chat.pending_state = None;
        chat.pending_confirmations = 0;
        return publish_chat_state(chat, next_state, next_activity_sequence) || metadata_changed;
    }

    let needs_confirmation = chat.had_active_turn
        && matches!(next_state, AgentChatState::Idle | AgentChatState::Quiet)
        && chat.state != next_state;
    if needs_confirmation {
        if chat.pending_state == Some(next_state) {
            chat.pending_confirmations = chat.pending_confirmations.saturating_add(1);
        } else {
            chat.pending_state = Some(next_state);
            chat.pending_confirmations = 1;
        }
        if chat.pending_confirmations < AGENT_STATE_CONFIRMATIONS {
            return metadata_changed;
        }
    }
    chat.pending_state = None;
    chat.pending_confirmations = 0;

    let completion = next_state == AgentChatState::Idle && chat.had_active_turn;
    let changed = publish_chat_state(chat, next_state, next_activity_sequence);
    if completion {
        chat.seen = chat.focused;
        chat.had_active_turn = false;
        return true;
    }
    changed || metadata_changed
}

fn chat_state_for_signal(signal: DetectionSignal) -> Option<AgentChatState> {
    match signal {
        DetectionSignal::Working => Some(AgentChatState::Working),
        DetectionSignal::NeedsInput => Some(AgentChatState::NeedsInput),
        DetectionSignal::Idle => Some(AgentChatState::Idle),
        DetectionSignal::Quiet => Some(AgentChatState::Quiet),
        DetectionSignal::Hold => None,
    }
}

fn publish_chat_state(
    chat: &mut AgentChat,
    state: AgentChatState,
    next_activity_sequence: &mut u64,
) -> bool {
    if chat.state == state {
        return false;
    }
    chat.state = state;
    chat.activity_sequence = next_agent_activity_sequence(next_activity_sequence);
    true
}

fn next_agent_activity_sequence(sequence: &mut u64) -> u64 {
    *sequence = sequence
        .checked_add(1)
        .expect("agent activity sequence exhausted");
    *sequence
}

pub(super) fn agent_chat_display_title(chat: &AgentChat) -> String {
    chat.custom_title
        .as_deref()
        .filter(|title| !title.trim().is_empty())
        .map(str::to_owned)
        .or_else(|| chat.prompt_snippet.clone())
        .or_else(|| chat.osc_title.clone())
        .unwrap_or_else(|| format!("{} chat #{}", chat.kind.label(), chat.creation_sequence))
}

pub(super) fn agent_chat_detail(chat: &AgentChat) -> String {
    let mut detail = format!("{} · {}", chat.state.label(chat.seen), chat.kind.label());
    if let Some(cwd) = chat
        .cwd
        .as_deref()
        .and_then(Path::file_name)
        .map(|name| name.to_string_lossy())
        .filter(|name| !name.is_empty())
    {
        detail.push_str(" · ");
        detail.push_str(&cwd);
    }
    detail
}

pub(super) fn agent_chat_tooltip(chat: &AgentChat, title: &str) -> String {
    let mut tooltip = format!(
        "Open {title}\n{} · {}",
        chat.state.label(chat.seen),
        chat.kind.label()
    );
    if let Some(cwd) = &chat.cwd {
        tooltip.push('\n');
        tooltip.push_str(&cwd.to_string_lossy());
    }
    tooltip
}

fn agent_chat_attention_priority(chat: &AgentChat) -> u8 {
    match (chat.state, chat.seen) {
        (AgentChatState::NeedsInput, _) => 5,
        (AgentChatState::Idle, false) => 4,
        (AgentChatState::Working, _) => 3,
        (AgentChatState::Quiet | AgentChatState::Open, _) => 2,
        (AgentChatState::Idle, true) => 1,
    }
}

fn sort_agent_chats(chats: &mut [AgentChat]) {
    chats.sort_by(|left, right| {
        agent_chat_attention_priority(right)
            .cmp(&agent_chat_attention_priority(left))
            .then(right.activity_sequence.cmp(&left.activity_sequence))
            .then(left.workspace_id.cmp(&right.workspace_id))
            .then(left.item_id.as_u64().cmp(&right.item_id.as_u64()))
    });
}

pub(super) fn agent_chats_for_workspace(
    chats: &HashMap<(WorkspaceId, EntityId), AgentChat>,
    workspace_id: WorkspaceId,
) -> Vec<AgentChat> {
    let mut chats = chats
        .values()
        .filter(|chat| chat.workspace_id == workspace_id)
        .cloned()
        .collect::<Vec<_>>();
    sort_agent_chats(&mut chats);
    chats
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_kinds_are_recognized_from_process_paths() {
        assert_eq!(AgentKind::from_process("claude"), Some(AgentKind::Claude));
        assert_eq!(
            AgentKind::from_process("/usr/local/bin/claude"),
            Some(AgentKind::Claude)
        );
        assert_eq!(
            AgentKind::from_process(r"C:\tools\claude-code.exe"),
            Some(AgentKind::Claude)
        );
        assert_eq!(AgentKind::from_process("codex"), Some(AgentKind::Codex));
        assert_eq!(
            AgentKind::from_process("opencode"),
            Some(AgentKind::OpenCode)
        );
        assert_eq!(AgentKind::from_process("pi"), Some(AgentKind::Pi));
        assert_eq!(AgentKind::from_process("amp"), Some(AgentKind::Amp));
        assert_eq!(AgentKind::from_process("gemini"), Some(AgentKind::Gemini));
        assert_eq!(AgentKind::from_process("aider"), Some(AgentKind::Aider));
        assert_eq!(AgentKind::from_process("goose"), Some(AgentKind::Goose));
    }

    #[test]
    fn non_agent_processes_are_excluded_from_the_agents_footer() {
        assert_eq!(AgentKind::from_process("nvim"), None);
        assert_eq!(AgentKind::from_process("git"), None);
        assert_eq!(AgentKind::from_process("cargo"), None);
        assert_eq!(AgentKind::from_process("bash"), None);
        // "pi" must match exactly; near-misses stay out.
        assert_eq!(AgentKind::from_process("pip"), None);
        assert_eq!(AgentKind::from_process("pixi"), None);
    }

    fn observation(
        item_id: EntityId,
        kind: AgentKind,
        recent: &str,
        osc_title: &str,
    ) -> AgentObservation {
        AgentObservation {
            kind,
            item_id,
            custom_title: None,
            osc_title: osc_title.into(),
            recent: recent.into(),
            cwd: Some(PathBuf::from("/tmp/zmux")),
        }
    }

    fn observed_workspace(
        agent: Option<AgentObservation>,
        item_id: EntityId,
        active: bool,
    ) -> AgentWorkspaceObservation {
        AgentWorkspaceObservation {
            agents: agent.into_iter().collect(),
            terminal_item_ids: vec![item_id],
            active_item_id: active.then_some(item_id),
        }
    }

    fn chat(
        item_id: EntityId,
        state: AgentChatState,
        seen: bool,
        activity_sequence: u64,
    ) -> AgentChat {
        AgentChat {
            workspace_id: 1,
            kind: AgentKind::Claude,
            item_id,
            custom_title: None,
            osc_title: None,
            prompt_snippet: None,
            cwd: None,
            state,
            seen,
            focused: false,
            had_active_turn: matches!(state, AgentChatState::Working | AgentChatState::NeedsInput),
            process_exited: false,
            creation_sequence: activity_sequence,
            activity_sequence,
            missing_refreshes: 0,
            pending_state: None,
            pending_confirmations: 0,
        }
    }

    #[test]
    fn coarse_agent_chat_is_retained_and_transitions_to_done_after_two_misses() {
        let item_id = EntityId::from(41_u64);
        let running = observed_workspace(
            Some(observation(item_id, AgentKind::OpenCode, "", "")),
            item_id,
            false,
        );
        let shell = observed_workspace(None, item_id, false);
        let mut chats = HashMap::new();
        let mut sequence = 0;

        assert!(reconcile_agent_chats_for_workspace(
            &mut chats,
            &mut sequence,
            7,
            &running,
        ));
        let chat = chats.get(&(7, item_id)).unwrap();
        assert_eq!(chat.state, AgentChatState::Open);
        assert_eq!(chat.activity_sequence, 1);

        assert!(reconcile_agent_chats_for_workspace(
            &mut chats,
            &mut sequence,
            7,
            &shell,
        ));
        assert_eq!(
            chats.get(&(7, item_id)).unwrap().state,
            AgentChatState::Open,
            "one missing process sample must not complete a live agent"
        );

        assert!(reconcile_agent_chats_for_workspace(
            &mut chats,
            &mut sequence,
            7,
            &shell,
        ));
        let chat = chats.get(&(7, item_id)).unwrap();
        assert_eq!(chat.state, AgentChatState::Idle);
        assert!(!chat.seen);
        assert_eq!(chat.activity_sequence, 2);

        let closed = AgentWorkspaceObservation::default();
        assert!(reconcile_agent_chats_for_workspace(
            &mut chats,
            &mut sequence,
            7,
            &closed,
        ));
        assert!(
            chats.is_empty(),
            "closing the terminal removes its chat row"
        );
    }

    #[test]
    fn detailed_agent_requires_two_idle_samples_before_marking_done() {
        let item_id = EntityId::from(42_u64);
        let working = observed_workspace(
            Some(observation(
                item_id,
                AgentKind::Codex,
                "› implement the rail",
                "⠋ implementing",
            )),
            item_id,
            false,
        );
        let idle = observed_workspace(
            Some(observation(
                item_id,
                AgentKind::Codex,
                "› implement the rail",
                "codex · zmux",
            )),
            item_id,
            false,
        );
        let focused_idle = AgentWorkspaceObservation {
            active_item_id: Some(item_id),
            ..idle.clone()
        };
        let mut chats = HashMap::new();
        let mut sequence = 0;

        reconcile_agent_chats_for_workspace(&mut chats, &mut sequence, 7, &working);
        let chat = chats.get(&(7, item_id)).unwrap();
        assert_eq!(chat.state, AgentChatState::Working);
        assert_eq!(chat.prompt_snippet.as_deref(), Some("implement the rail"));

        reconcile_agent_chats_for_workspace(&mut chats, &mut sequence, 7, &idle);
        assert_eq!(
            chats.get(&(7, item_id)).unwrap().state,
            AgentChatState::Working
        );

        reconcile_agent_chats_for_workspace(&mut chats, &mut sequence, 7, &idle);
        let chat = chats.get(&(7, item_id)).unwrap();
        assert_eq!(chat.state, AgentChatState::Idle);
        assert!(!chat.seen);

        reconcile_agent_chats_for_workspace(&mut chats, &mut sequence, 7, &focused_idle);
        assert!(chats.get(&(7, item_id)).unwrap().seen);
    }

    #[test]
    fn agent_chats_sort_by_attention_then_most_recent_activity() {
        let needs_input_id = EntityId::from(1_u64);
        let older_completed_id = EntityId::from(2_u64);
        let newest_completed_id = EntityId::from(3_u64);
        let working_id = EntityId::from(4_u64);
        let quiet_id = EntityId::from(5_u64);
        let seen_idle_id = EntityId::from(6_u64);
        let mut chats = vec![
            chat(working_id, AgentChatState::Working, true, 20),
            chat(older_completed_id, AgentChatState::Idle, false, 4),
            chat(newest_completed_id, AgentChatState::Idle, false, 8),
            chat(needs_input_id, AgentChatState::NeedsInput, true, 2),
            chat(quiet_id, AgentChatState::Quiet, true, 30),
            chat(seen_idle_id, AgentChatState::Idle, true, 40),
        ];

        sort_agent_chats(&mut chats);

        assert_eq!(
            chats.iter().map(|chat| chat.item_id).collect::<Vec<_>>(),
            vec![
                needs_input_id,
                newest_completed_id,
                older_completed_id,
                working_id,
                quiet_id,
                seen_idle_id,
            ]
        );
    }

    #[test]
    fn chat_list_contains_only_the_selected_workspace() {
        let first_item = EntityId::from(11_u64);
        let second_item = EntityId::from(12_u64);
        let second_chat = AgentChat {
            workspace_id: 2,
            ..chat(second_item, AgentChatState::Idle, false, 2)
        };
        let chats = [
            chat(first_item, AgentChatState::Working, true, 1),
            second_chat,
        ]
        .into_iter()
        .map(|chat| ((chat.workspace_id, chat.item_id), chat))
        .collect();

        let visible = agent_chats_for_workspace(&chats, 1);

        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].workspace_id, 1);
        assert_eq!(visible[0].item_id, first_item);
    }

    #[test]
    fn agent_chat_titles_prefer_custom_prompt_and_osc_metadata() {
        let mut chat = chat(EntityId::from(1_u64), AgentChatState::Working, true, 1);
        chat.kind = AgentKind::Codex;
        assert_eq!(agent_chat_display_title(&chat), "Codex chat #1");

        chat.osc_title = Some("refactor workspace state".into());
        assert_eq!(agent_chat_display_title(&chat), "refactor workspace state");
        chat.prompt_snippet = Some("implement the agent rail".into());
        assert_eq!(agent_chat_display_title(&chat), "implement the agent rail");
        chat.custom_title = Some("primary chat".into());
        assert_eq!(agent_chat_display_title(&chat), "primary chat");
    }
}
