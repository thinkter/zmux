//! Incremental parsing for terminal notification OSC sequences.
//!
//! Zed's terminal abstraction currently does not expose raw OSC payloads to
//! downstream applications. Keeping the parser independent still gives the
//! terminal integration a small, well-tested surface to call when that hook is
//! available, and lets other transports (for example an agent adapter) use the
//! exact same semantics.

use crate::notifications::{NotificationLevel, NotificationSource};

/// Never retain arbitrary terminal output forever while waiting for a missing
/// BEL/ST terminator.
pub const MAX_PENDING_OSC_BYTES: usize = 16 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OscNotification {
    pub source: NotificationSource,
    pub level: NotificationLevel,
    pub title: String,
    pub body: String,
}

/// A streaming parser. `push` may be called with arbitrarily split PTY chunks.
#[derive(Default)]
pub struct OscNotificationParser {
    pending: Vec<u8>,
}

impl OscNotificationParser {
    pub fn push(&mut self, bytes: &[u8]) -> Vec<OscNotification> {
        self.pending.extend_from_slice(bytes);
        let mut notifications = Vec::new();

        loop {
            let Some(start) = find_osc_start(&self.pending) else {
                // Preserve a trailing ESC since the next PTY chunk may begin
                // with `]`; ordinary terminal output is irrelevant here.
                if self.pending.last() == Some(&0x1b) {
                    self.pending.drain(..self.pending.len().saturating_sub(1));
                } else {
                    self.pending.clear();
                }
                break;
            };

            if start > 0 {
                self.pending.drain(..start);
            }

            let Some((end, terminator_len)) = find_osc_terminator(&self.pending[2..]) else {
                if self.pending.len() > MAX_PENDING_OSC_BYTES {
                    // Drop this malformed/incomplete sequence wholesale rather
                    // than keeping potentially unbounded shell output.
                    self.pending.clear();
                }
                break;
            };

            let payload = self.pending[2..2 + end].to_vec();
            self.pending.drain(..2 + end + terminator_len);
            if let Some(notification) = parse_osc_payload(&payload) {
                notifications.push(notification);
            }
        }

        notifications
    }
}

pub fn parse_osc_payload(payload: &[u8]) -> Option<OscNotification> {
    let text = std::str::from_utf8(payload).ok()?;
    let mut fields = text.split(';');
    let code = fields.next()?;
    let fields: Vec<_> = fields.collect();

    match code {
        // OSC 9;body. This is the most common cross-terminal notification
        // convention and intentionally uses a stable fallback title.
        "9" => Some(OscNotification {
            source: NotificationSource::Osc9,
            level: NotificationLevel::Info,
            title: "Terminal notification".to_string(),
            body: fields.join(";"),
        }),
        // OSC 99 is used by Windows Terminal's notification extension. Accept
        // both `99;title;body` and `99;notify;title;body` for interoperability.
        "99" => parse_extended(NotificationSource::Osc99, &fields),
        // iTerm-compatible OSC 777 notifications are conventionally
        // `777;notify;title;body`.
        "777" => parse_extended(NotificationSource::Osc777, &fields),
        _ => None,
    }
}

fn parse_extended(source: NotificationSource, fields: &[&str]) -> Option<OscNotification> {
    let fields = if fields.first().is_some_and(|field| *field == "notify") {
        &fields[1..]
    } else {
        fields
    };
    let title = fields.first()?.to_string();
    let body = fields.get(1..).unwrap_or_default().join(";");
    Some(OscNotification {
        source,
        level: NotificationLevel::Info,
        title,
        body,
    })
}

fn find_osc_start(bytes: &[u8]) -> Option<usize> {
    bytes.windows(2).position(|window| window == [0x1b, b']'])
}

/// Returns the payload length and the terminating sequence length. The caller
/// has already skipped the `ESC ]` introducer.
fn find_osc_terminator(bytes: &[u8]) -> Option<(usize, usize)> {
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            0x07 => return Some((index, 1)),
            0x1b if bytes.get(index + 1) == Some(&b'\\') => return Some((index, 2)),
            _ => index += 1,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_osc_9_bel_terminated_notifications() {
        let mut parser = OscNotificationParser::default();
        assert_eq!(
            parser.push(b"build\x1b]9;done\x07"),
            vec![OscNotification {
                source: NotificationSource::Osc9,
                level: NotificationLevel::Info,
                title: "Terminal notification".to_string(),
                body: "done".to_string(),
            }]
        );
    }

    #[test]
    fn supports_chunked_st_terminated_osc_777() {
        let mut parser = OscNotificationParser::default();
        assert!(parser.push(b"\x1b]777;notify;Build").is_empty());
        assert_eq!(
            parser.push(b";tests passed\x1b\\"),
            vec![OscNotification {
                source: NotificationSource::Osc777,
                level: NotificationLevel::Info,
                title: "Build".to_string(),
                body: "tests passed".to_string(),
            }]
        );
    }

    #[test]
    fn accepts_windows_terminal_osc_99_variants() {
        assert_eq!(
            parse_osc_payload(b"99;notify;Codex;waiting"),
            Some(OscNotification {
                source: NotificationSource::Osc99,
                level: NotificationLevel::Info,
                title: "Codex".to_string(),
                body: "waiting".to_string(),
            })
        );
        assert_eq!(
            parse_osc_payload(b"99;Codex;waiting"),
            Some(OscNotification {
                source: NotificationSource::Osc99,
                level: NotificationLevel::Info,
                title: "Codex".to_string(),
                body: "waiting".to_string(),
            })
        );
    }

    #[test]
    fn discards_unterminated_payloads_at_the_memory_bound() {
        let mut parser = OscNotificationParser::default();
        let mut payload = b"\x1b]9;".to_vec();
        payload.extend(std::iter::repeat_n(b'x', MAX_PENDING_OSC_BYTES));
        assert!(parser.push(&payload).is_empty());
        assert_eq!(parser.pending.len(), 0);
    }
}
