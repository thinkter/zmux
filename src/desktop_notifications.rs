//! Best-effort native desktop notification delivery.
//!
//! This module deliberately has no GUI or IPC dependency. The notification
//! store decides what happened; this adapter decides whether the operating
//! system should be asked to surface it. Missing host tools are reported as an
//! explicit unavailable result rather than treated as a successful alert.

use std::process::Command;

use crate::notifications::NotificationLevel;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DesktopNotification {
    pub title: String,
    pub subtitle: Option<String>,
    pub body: String,
    pub level: NotificationLevel,
}

impl DesktopNotification {
    pub fn new(
        title: impl Into<String>,
        subtitle: Option<String>,
        body: impl Into<String>,
        level: NotificationLevel,
    ) -> Self {
        Self {
            title: title.into(),
            subtitle,
            body: body.into(),
            level,
        }
    }
}

/// Delivery policy belongs to zmux configuration rather than the transport or
/// a terminal emulator. This lets callers suppress a pop-up for an active,
/// focused workspace while retaining the event in the notification history.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DesktopNotificationPolicy {
    pub suppress_when_focused: bool,
}

impl Default for DesktopNotificationPolicy {
    fn default() -> Self {
        Self {
            suppress_when_focused: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DesktopDelivery {
    Delivered,
    SuppressedBecauseFocused,
    Unavailable { reason: String },
    Failed { reason: String },
}

/// Deliver an alert if policy permits. This is synchronous by design: callers
/// should invoke it from a background task, not hold a UI update while a host
/// notification command starts.
pub fn deliver_desktop_notification(
    notification: &DesktopNotification,
    app_is_focused: bool,
    policy: DesktopNotificationPolicy,
) -> DesktopDelivery {
    if app_is_focused && policy.suppress_when_focused {
        return DesktopDelivery::SuppressedBecauseFocused;
    }

    let Some(mut command) = platform_command(notification) else {
        return DesktopDelivery::Unavailable {
            reason: "desktop notifications are not supported on this platform".to_string(),
        };
    };

    match command.status() {
        Ok(status) if status.success() => DesktopDelivery::Delivered,
        Ok(status) => DesktopDelivery::Failed {
            reason: format!("desktop notification command exited with {status}"),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            DesktopDelivery::Unavailable {
                reason: format!("desktop notification command is unavailable: {error}"),
            }
        }
        Err(error) => DesktopDelivery::Failed {
            reason: format!("failed to start desktop notification command: {error}"),
        },
    }
}

#[cfg(target_os = "linux")]
fn platform_command(notification: &DesktopNotification) -> Option<Command> {
    let mut command = Command::new("notify-send");
    command
        .arg("--app-name=zmux")
        .arg(format!("--urgency={}", linux_urgency(notification.level)))
        .arg(bounded_text(&notification.title))
        .arg(bounded_text(&notification.body));
    Some(command)
}

#[cfg(target_os = "macos")]
fn platform_command(notification: &DesktopNotification) -> Option<Command> {
    let title = apple_script_string(&bounded_text(&notification.title));
    let body = apple_script_string(&bounded_text(&notification.body));
    let subtitle = notification
        .subtitle
        .as_deref()
        .filter(|subtitle| !subtitle.is_empty())
        .map(bounded_text)
        .map(|subtitle| format!(" subtitle \"{}\"", apple_script_string(&subtitle)))
        .unwrap_or_default();
    let script = format!("display notification \"{body}\" with title \"{title}\"{subtitle}",);
    let mut command = Command::new("osascript");
    command.arg("-e").arg(script);
    Some(command)
}

#[cfg(target_os = "windows")]
fn platform_command(notification: &DesktopNotification) -> Option<Command> {
    // Windows 10+ exposes toast notifications through the Windows Runtime. Use
    // a self-contained PowerShell command rather than assuming a third-party
    // module such as BurntToast is installed. Values are passed through process
    // environment rather than interpolated into executable script text.
    let script = r#"
Add-Type -AssemblyName System.Runtime.WindowsRuntime
$template = [Windows.UI.Notifications.ToastTemplateType, Windows.UI.Notifications, ContentType=WindowsRuntime]::ToastText02
$xml = [Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType=WindowsRuntime]::GetTemplateContent($template)
$text = $xml.GetElementsByTagName('text')
$text.Item(0).AppendChild($xml.CreateTextNode($env:ZMUX_NOTIFICATION_TITLE)) | Out-Null
$text.Item(1).AppendChild($xml.CreateTextNode($env:ZMUX_NOTIFICATION_BODY)) | Out-Null
$toast = [Windows.UI.Notifications.ToastNotification, Windows.UI.Notifications, ContentType=WindowsRuntime]::new($xml)
[Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType=WindowsRuntime]::CreateToastNotifier('zmux').Show($toast)
"#;
    let mut command = Command::new("powershell.exe");
    command
        .arg("-NoProfile")
        .arg("-NonInteractive")
        .arg("-Command")
        .arg(script)
        .env("ZMUX_NOTIFICATION_TITLE", bounded_text(&notification.title))
        .env("ZMUX_NOTIFICATION_BODY", bounded_text(&notification.body));
    Some(command)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn platform_command(_notification: &DesktopNotification) -> Option<Command> {
    None
}

fn bounded_text(text: &str) -> String {
    const MAX_DESKTOP_TEXT_CHARS: usize = 4_096;
    let mut chars = text.chars();
    let value: String = chars.by_ref().take(MAX_DESKTOP_TEXT_CHARS).collect();
    if chars.next().is_some() {
        format!("{value}…")
    } else {
        value
    }
}

#[cfg(target_os = "linux")]
fn linux_urgency(level: NotificationLevel) -> &'static str {
    match level {
        NotificationLevel::Info | NotificationLevel::Success => "normal",
        NotificationLevel::Warning => "normal",
        NotificationLevel::Error => "critical",
    }
}

#[cfg(target_os = "macos")]
fn apple_script_string(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notification() -> DesktopNotification {
        DesktopNotification::new(
            "Build complete",
            Some("zmux".to_string()),
            "tests passed",
            NotificationLevel::Success,
        )
    }

    #[test]
    fn focus_suppression_does_not_invoke_the_platform() {
        assert_eq!(
            deliver_desktop_notification(
                &notification(),
                true,
                DesktopNotificationPolicy::default(),
            ),
            DesktopDelivery::SuppressedBecauseFocused
        );
    }

    #[test]
    fn bounded_text_preserves_unicode_boundaries() {
        let value = "🙂".repeat(4_097);
        let bounded = bounded_text(&value);
        assert_eq!(bounded.chars().count(), 4_097);
        assert!(bounded.ends_with('…'));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_urgency_escalates_errors() {
        assert_eq!(linux_urgency(NotificationLevel::Info), "normal");
        assert_eq!(linux_urgency(NotificationLevel::Error), "critical");
    }
}
