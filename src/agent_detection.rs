//! Zero-configuration status detection for agent CLIs running in terminals.
//!
//! The detector only consumes a bounded live terminal tail and the latest OSC
//! title. It deliberately returns `Quiet` or `Hold` when the visible evidence
//! is ambiguous instead of claiming that an agent is thinking or finished.

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum AgentKind {
    Claude,
    Codex,
    OpenCode,
    Pi,
    Amp,
    Gemini,
    Aider,
    Goose,
}

impl AgentKind {
    pub(crate) fn from_process(process: &str) -> Option<Self> {
        let executable = process
            .trim()
            .rsplit(['/', '\\'])
            .next()?
            .to_ascii_lowercase();
        let executable = executable.strip_suffix(".exe").unwrap_or(&executable);

        match executable {
            "claude" | "claude-code" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            "opencode" => Some(Self::OpenCode),
            "pi" => Some(Self::Pi),
            "amp" => Some(Self::Amp),
            "gemini" => Some(Self::Gemini),
            "aider" => Some(Self::Aider),
            "goose" => Some(Self::Goose),
            _ => None,
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Claude => "Claude",
            Self::Codex => "Codex",
            Self::OpenCode => "opencode",
            Self::Pi => "Pi",
            Self::Amp => "Amp",
            Self::Gemini => "Gemini",
            Self::Aider => "Aider",
            Self::Goose => "Goose",
        }
    }

    pub(crate) fn has_detailed_detection(self) -> bool {
        matches!(self, Self::Claude | Self::Codex)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DetectionSignal {
    Working,
    NeedsInput,
    Idle,
    Quiet,
    /// A transient viewer or picker is covering the live agent UI. Preserve
    /// the previously published state until live evidence returns.
    Hold,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DetectionOutcome {
    pub(crate) signal: DetectionSignal,
    pub(crate) evidence: &'static str,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct AgentSnapshot<'a> {
    pub(crate) recent: &'a str,
    pub(crate) osc_title: &'a str,
}

pub(crate) fn detect_agent(kind: AgentKind, snapshot: AgentSnapshot<'_>) -> DetectionOutcome {
    match kind {
        AgentKind::Codex => detect_codex(snapshot),
        AgentKind::Claude => detect_claude(snapshot),
        _ => DetectionOutcome {
            signal: DetectionSignal::Quiet,
            evidence: "process_only",
        },
    }
}

fn detect_codex(snapshot: AgentSnapshot<'_>) -> DetectionOutcome {
    let title = snapshot.osc_title.trim();
    if contains_ci(title, "action required") {
        return outcome(DetectionSignal::NeedsInput, "codex_title_action_required");
    }
    if starts_with_braille_spinner(title) {
        return outcome(DetectionSignal::Working, "codex_title_spinner");
    }

    let after_prompt = after_last_line_marker(snapshot.recent, codex_prompt_line);
    if codex_transcript_viewer(after_prompt) {
        return outcome(DetectionSignal::Hold, "codex_transcript_viewer");
    }
    if [
        "press enter to confirm or esc to cancel",
        "enter to submit answer",
        "enter to submit all",
        "allow command?",
    ]
    .iter()
    .any(|needle| contains_ci(after_prompt, needle))
    {
        return outcome(DetectionSignal::NeedsInput, "codex_live_prompt");
    }

    let lower = snapshot.recent.to_lowercase();
    let weak_question = (lower.contains("do you want to") || lower.contains("would you like to"))
        && (lower.contains("yes") || snapshot.recent.contains('❯'));
    if lower.contains("[y/n]") || lower.contains("yes (y)") || weak_question {
        return outcome(DetectionSignal::NeedsInput, "codex_visible_question");
    }

    if !title.is_empty() {
        return outcome(DetectionSignal::Idle, "codex_stable_title");
    }

    outcome(DetectionSignal::Quiet, "codex_no_live_signal")
}

fn detect_claude(snapshot: AgentSnapshot<'_>) -> DetectionOutcome {
    let title = snapshot.osc_title.trim();
    if starts_with_braille_spinner(title) {
        return outcome(DetectionSignal::Working, "claude_title_spinner");
    }

    let lower = snapshot.recent.to_lowercase();
    if claude_transcript_viewer(&lower) {
        return outcome(DetectionSignal::Hold, "claude_transcript_viewer");
    }
    if lower.contains("select model")
        && lower.contains("enter to set as default")
        && lower.contains("esc to cancel")
    {
        return outcome(DetectionSignal::Hold, "claude_model_picker");
    }

    let live_form = after_last_horizontal_rule(snapshot.recent);
    let live_form_lower = live_form.to_lowercase();
    let navigation = [
        "tab/arrow keys to navigate",
        "arrow keys to navigate",
        "arrows to navigate",
        "↑/↓ to navigate",
        "↑↓ to navigate",
    ]
    .iter()
    .any(|needle| live_form_lower.contains(needle));
    if live_form_lower.contains("enter to select")
        && live_form_lower.contains("esc to cancel")
        && navigation
    {
        return outcome(DetectionSignal::NeedsInput, "claude_live_form");
    }
    if lower.contains("run a dynamic workflow?") && lower.contains("esc to cancel") {
        return outcome(DetectionSignal::NeedsInput, "claude_workflow_prompt");
    }

    let permission_prompt = lower.contains("do you want to proceed?")
        && (lower.contains("esc to cancel")
            || lower.contains("bash command")
            || lower.contains("tab to amend")
            || lower.contains("ctrl+e to explain"))
        && (lower.contains("yes") || snapshot.recent.contains('❯'));
    let legacy_prompt = lower.contains("waiting for permission")
        || lower.contains("do you want to allow this connection?")
        || lower.contains("review your answers")
        || lower.contains("skip interview and plan immediately");
    if permission_prompt || legacy_prompt {
        return outcome(DetectionSignal::NeedsInput, "claude_permission_prompt");
    }

    if let Some(prompt_box) = prompt_box_body(snapshot.recent) {
        let prompt_box_lower = prompt_box.to_lowercase();
        let has_prompt = prompt_box
            .lines()
            .any(|line| line.trim_start().starts_with('❯'));
        let is_menu = prompt_box_lower.contains("enter to select")
            || prompt_box_lower.contains("esc to cancel")
            || prompt_box_lower.contains("arrow keys")
            || prompt_box_lower.contains("↑/↓ to navigate");
        if has_prompt && !is_menu {
            return outcome(DetectionSignal::Idle, "claude_prompt_box");
        }
    }

    if title.starts_with("✳ ") || title == "✳" {
        return outcome(DetectionSignal::Idle, "claude_idle_title");
    }

    outcome(DetectionSignal::Quiet, "claude_no_live_signal")
}

pub(crate) fn submitted_prompt(kind: AgentKind, recent: &str) -> Option<String> {
    let marker = match kind {
        AgentKind::Codex => '›',
        AgentKind::Claude => '❯',
        _ => return None,
    };

    recent.lines().rev().find_map(|line| {
        let trimmed = line.trim_start();
        let text = trimmed.strip_prefix(marker)?.trim();
        if text.is_empty() || prompt_control_text(text) {
            return None;
        }
        bounded_normalized(text, 96)
    })
}

pub(crate) fn sanitized_osc_title(kind: AgentKind, title: &str) -> Option<String> {
    let mut title = title.trim();
    if title.is_empty() || contains_ci(title, "action required") {
        return None;
    }

    if title.chars().next().is_some_and(is_braille) {
        title = title.chars().next().map_or(title, |spinner| {
            title.strip_prefix(spinner).unwrap_or(title).trim_start()
        });
    }
    if let Some(rest) = title.strip_prefix('✳') {
        title = rest.trim_start();
    }
    if title.is_empty() || AgentKind::from_process(title) == Some(kind) {
        return None;
    }
    bounded_normalized(title, 96)
}

fn outcome(signal: DetectionSignal, evidence: &'static str) -> DetectionOutcome {
    DetectionOutcome { signal, evidence }
}

/// Allocation-free ASCII-folding search. Non-ASCII bytes (such as UI arrows)
/// are compared exactly, and this runs when terminal events invalidate detection.
fn contains_ci(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    haystack
        .as_bytes()
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
}

fn starts_with_braille_spinner(text: &str) -> bool {
    let mut chars = text.chars();
    chars.next().is_some_and(is_braille) && chars.next().is_some_and(char::is_whitespace)
}

fn is_braille(ch: char) -> bool {
    ('\u{2800}'..='\u{28ff}').contains(&ch)
}

fn codex_prompt_line(line: &str) -> bool {
    let line = line.trim_start();
    line == "›" || line.starts_with("› ")
}

fn after_last_line_marker(content: &str, marker: impl Fn(&str) -> bool) -> &str {
    let offset = content
        .match_indices('\n')
        .map(|(index, _)| index + 1)
        .chain(std::iter::once(0))
        .filter(|offset| marker(content[*offset..].lines().next().unwrap_or_default()))
        .max();
    offset.map_or(content, |offset| {
        let line_end = content[offset..]
            .find('\n')
            .map(|index| offset + index + 1)
            .unwrap_or(content.len());
        &content[line_end..]
    })
}

fn codex_transcript_viewer(content: &str) -> bool {
    contains_ci(content, "↑/↓ to scroll")
        && contains_ci(content, "q to quit")
        && (contains_ci(content, "esc to edit prev") || contains_ci(content, "esc/← to edit prev"))
}

fn claude_transcript_viewer(lower: &str) -> bool {
    lower.contains("showing detailed transcript")
        && (lower.contains("ctrl+o")
            || lower.contains("ctrl+e")
            || lower.contains("↑↓ scroll")
            || lower.contains("? for shortcuts"))
}

fn prompt_control_text(text: &str) -> bool {
    let lower = text.to_lowercase();
    lower.chars().next().is_some_and(|ch| ch.is_ascii_digit())
        || lower.contains("enter to select")
        || lower.contains("esc to cancel")
        || lower.contains("arrow keys")
}

fn bounded_normalized(text: &str, max_chars: usize) -> Option<String> {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        return None;
    }
    if normalized.chars().count() <= max_chars {
        return Some(normalized);
    }
    let mut bounded = normalized
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect::<String>();
    while bounded.ends_with(char::is_whitespace) {
        bounded.pop();
    }
    bounded.push('…');
    Some(bounded)
}

fn after_last_horizontal_rule(content: &str) -> &str {
    let mut current = 0;
    let mut last_rule_end = 0;
    for line in content.lines() {
        let next = (current + line.len() + 1).min(content.len());
        if is_horizontal_rule(line) {
            last_rule_end = next;
        }
        current = next;
    }
    &content[last_rule_end..]
}

fn prompt_box_body(content: &str) -> Option<&str> {
    let lines = content.lines().collect::<Vec<_>>();
    let borders = lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| is_horizontal_rule(line).then_some(index))
        .collect::<Vec<_>>();
    let top = *borders.iter().rev().nth(1)?;
    let bottom = borders
        .into_iter()
        .find(|index| *index > top)
        .unwrap_or(lines.len());
    let start = line_offset(content, &lines, top + 1);
    let end = line_offset(content, &lines, bottom);
    Some(&content[start.min(content.len())..end.min(content.len())])
}

fn line_offset(content: &str, lines: &[&str], index: usize) -> usize {
    lines
        .iter()
        .take(index)
        .map(|line| line.len() + 1)
        .sum::<usize>()
        .min(content.len())
}

fn is_horizontal_rule(line: &str) -> bool {
    let trimmed = line.trim();
    let count = trimmed.chars().take_while(|ch| *ch == '─').count();
    count >= 3
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot<'a>(recent: &'a str, title: &'a str) -> AgentSnapshot<'a> {
        AgentSnapshot {
            recent,
            osc_title: title,
        }
    }

    #[test]
    fn codex_uses_live_title_and_prompt_evidence() {
        assert_eq!(
            detect_agent(AgentKind::Codex, snapshot("", "⠋ implementing")).signal,
            DetectionSignal::Working
        );
        assert_eq!(
            detect_agent(AgentKind::Codex, snapshot("", "Action Required")).signal,
            DetectionSignal::NeedsInput
        );
        assert_eq!(
            detect_agent(AgentKind::Codex, snapshot("› fix it\nAllow command?\n", "")).signal,
            DetectionSignal::NeedsInput
        );
        assert_eq!(
            detect_agent(AgentKind::Codex, snapshot("› ", "codex · zmux")).signal,
            DetectionSignal::Idle
        );
    }

    #[test]
    fn codex_transcript_viewer_holds_the_previous_state() {
        let recent = "› prompt\n↑/↓ to scroll · esc to edit prev · q to quit";
        assert_eq!(
            detect_agent(AgentKind::Codex, snapshot(recent, "")).signal,
            DetectionSignal::Hold
        );
    }

    #[test]
    fn claude_distinguishes_working_blocked_idle_and_transient_views() {
        assert_eq!(
            detect_agent(AgentKind::Claude, snapshot("", "⢀ editing")).signal,
            DetectionSignal::Working
        );
        let permission = "────────\nDo you want to proceed?\n❯ 1. Yes\n2. No\nEsc to cancel";
        assert_eq!(
            detect_agent(AgentKind::Claude, snapshot(permission, "")).signal,
            DetectionSignal::NeedsInput
        );
        let idle = "────────\n❯ \n────────";
        assert_eq!(
            detect_agent(AgentKind::Claude, snapshot(idle, "")).signal,
            DetectionSignal::Idle
        );
        let picker = "Select model\nEnter to set as default\nEsc to cancel";
        assert_eq!(
            detect_agent(AgentKind::Claude, snapshot(picker, "")).signal,
            DetectionSignal::Hold
        );
    }

    #[test]
    fn unmatched_detailed_agents_are_reported_as_quiet() {
        assert_eq!(
            detect_agent(AgentKind::Codex, snapshot("ordinary output", "")).signal,
            DetectionSignal::Quiet
        );
        assert_eq!(
            detect_agent(AgentKind::Claude, snapshot("ordinary output", "")).signal,
            DetectionSignal::Quiet
        );
    }

    #[test]
    fn prompt_and_title_metadata_are_bounded_and_status_free() {
        assert_eq!(
            submitted_prompt(AgentKind::Codex, "› explain the failing test"),
            Some("explain the failing test".into())
        );
        assert_eq!(
            submitted_prompt(AgentKind::Claude, "❯ review the API"),
            Some("review the API".into())
        );
        assert_eq!(
            sanitized_osc_title(AgentKind::Codex, "Action Required"),
            None
        );
        assert_eq!(
            sanitized_osc_title(AgentKind::Codex, "⠋ refactor workspace state"),
            Some("refactor workspace state".into())
        );
        assert!(
            submitted_prompt(AgentKind::Codex, &format!("› {}", "x".repeat(200)))
                .unwrap()
                .chars()
                .count()
                <= 96
        );
    }
}
