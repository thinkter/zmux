//! Terminal notification OSC decoding.
//!
//! The pinned Zed terminal stack consumes unknown OSC commands before zmux can
//! observe them. The small vendored VTE/Alacritty patches bridge OSC 9, 99, and
//! 777 through a reserved, transient title event. This module recognizes that
//! marker, validates the original payload, assembles Kitty OSC 99 chunks, and
//! returns semantic events for the application notification pipeline.

use std::collections::{HashMap, VecDeque};
use std::error::Error;
use std::fmt::{self, Display, Formatter};

use crate::notifications::{NotificationLevel, NotificationSource};

/// Must remain identical to the constant in the vendored `vte` patch.
pub const OSC_NOTIFICATION_TITLE_PREFIX: &str = "\u{1f}zmux-osc-notification-v1:";

/// Upper bound enforced by both the VTE bridge and this decoder.
pub const MAX_BRIDGED_OSC_BYTES: usize = 8 * 1024;
/// Maximum real terminal title carried only long enough to restore breadcrumbs.
pub const MAX_BRIDGE_RESTORE_TITLE_BYTES: usize = 64 * 1024;
/// Maximum replay envelope emitted by the vendored Alacritty bridge.
pub const MAX_BRIDGE_ENVELOPE_BYTES: usize = 1024 * 1024;
/// Maximum unacknowledged entries retained in one replay envelope.
pub const MAX_BRIDGE_REPLAY_ENTRIES: usize = 128;
/// Kitty's protocol limit for one unencoded payload chunk.
pub const MAX_KITTY_PLAIN_CHUNK_BYTES: usize = 2_048;
/// Kitty's protocol limit for one Base64-encoded payload chunk.
pub const MAX_KITTY_ENCODED_CHUNK_BYTES: usize = 4_096;
/// Aggregate bound for a chunked notification before it is published.
pub const MAX_OSC_NOTIFICATION_BYTES: usize = 64 * 1024;
/// Maximum payload frames accepted while assembling one Kitty notification.
///
/// Kitty permits terminals to impose a sensible denial-of-service limit. This
/// is four times the 32 maximum-size plain chunks needed to reach zmux's 64 KiB
/// aggregate limit, leaving room for smaller chunks without allowing empty
/// `d=0` frames to grow retained state indefinitely.
pub const MAX_KITTY_NOTIFICATION_FRAMES: usize = 128;
/// Number of incomplete Kitty notification IDs retained per terminal.
pub const MAX_PENDING_KITTY_NOTIFICATIONS: usize = 32;

const MAX_KITTY_IDENTIFIER_BYTES: usize = 128;
const MAX_KITTY_METADATA_VALUE_BYTES: usize = 4_096;
const MAX_KITTY_METADATA_TEXT_BYTES: usize = 1_024;
const MAX_KITTY_METADATA_ITEMS: usize = 16;

/// A fully assembled notification request emitted by a terminal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OscNotification {
    pub source: NotificationSource,
    pub level: NotificationLevel,
    pub title: String,
    pub body: String,
    pub kitty: Option<KittyNotificationMetadata>,
}

/// Kitty OSC 99 metadata relevant to presentation and later activation support.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KittyNotificationMetadata {
    pub identifier: Option<String>,
    pub application_name: Option<String>,
    pub notification_types: Vec<String>,
    pub urgency: KittyUrgency,
    pub delivery_condition: KittyDeliveryCondition,
    pub activation: KittyActivation,
    pub request_close_report: bool,
    pub sound_name: Option<String>,
    pub close_after_ms: Option<u32>,
    pub icon_identifier: Option<String>,
    pub icon_names: Vec<String>,
    pub buttons: Vec<String>,
}

impl Default for KittyNotificationMetadata {
    fn default() -> Self {
        Self {
            identifier: None,
            application_name: None,
            notification_types: Vec::new(),
            urgency: KittyUrgency::Normal,
            delivery_condition: KittyDeliveryCondition::Always,
            activation: KittyActivation::default(),
            request_close_report: false,
            sound_name: None,
            close_after_ms: None,
            icon_identifier: None,
            icon_names: Vec::new(),
            buttons: Vec::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum KittyUrgency {
    Low,
    #[default]
    Normal,
    Critical,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum KittyDeliveryCondition {
    #[default]
    Always,
    Unfocused,
    Invisible,
}

/// Requested behavior when the user activates a native notification.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KittyActivation {
    pub focus: bool,
    pub report: bool,
}

impl Default for KittyActivation {
    fn default() -> Self {
        Self {
            focus: true,
            report: false,
        }
    }
}

/// OSC 99 contains management/query messages in addition to notifications.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OscNotificationEvent {
    Notification(Box<OscNotification>),
    Close { identifier: String },
    AliveQuery { identifier: Option<String> },
    CapabilityQuery { identifier: Option<String> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OscParseError {
    MarkerTooLarge,
    MalformedSequence,
    InvalidMetadata,
    InvalidIdentifier,
    InvalidPayload,
    InvalidBase64,
    PayloadTooLarge,
    NotificationTooLarge,
}

impl Display for OscParseError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::MarkerTooLarge => "OSC notification marker exceeds its size limit",
            Self::MalformedSequence => "malformed OSC notification sequence",
            Self::InvalidMetadata => "invalid Kitty OSC 99 metadata",
            Self::InvalidIdentifier => "invalid Kitty OSC 99 identifier",
            Self::InvalidPayload => "invalid OSC notification payload",
            Self::InvalidBase64 => "invalid Base64 in Kitty OSC 99 payload",
            Self::PayloadTooLarge => "OSC notification chunk exceeds its size limit",
            Self::NotificationTooLarge => "chunked OSC notification exceeds its size limit",
        })
    }
}

impl Error for OscParseError {}

/// Stateful decoder. Keep one parser per terminal surface so Kitty IDs from
/// unrelated PTYs cannot collide.
#[derive(Clone, Default)]
pub struct OscNotificationParser {
    pending: HashMap<String, PendingKittyNotification>,
    pending_order: VecDeque<String>,
    bridge_sequence_watermark: Option<u64>,
}

impl OscNotificationParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Process a terminal title change. Ordinary titles return an empty list.
    pub fn push_title(&mut self, title: &str) -> Result<Vec<OscNotificationEvent>, OscParseError> {
        let Some(envelope) = decode_bridged_osc_title(title)? else {
            return Ok(Vec::new());
        };
        // A replay envelope is one delivery transaction. Stage both sequence
        // progress and Kitty chunk state so an error in a later frame cannot
        // consume earlier valid frames without returning their events.
        let mut staged = self.clone();
        let mut events = Vec::new();
        for frame in envelope.frames {
            if let Some(sequence) = frame.sequence {
                if staged
                    .bridge_sequence_watermark
                    .is_some_and(|seen| !bridge_sequence_is_after(sequence, seen))
                {
                    continue;
                }
                // Advance the staged sequence before parsing so later frames
                // compare against it. Nothing is committed unless every new
                // frame in this replay envelope succeeds.
                staged.bridge_sequence_watermark = Some(sequence);
            }
            if let Some(event) = staged.push_payload(frame.payload)? {
                events.push(event);
            }
        }
        *self = staged;
        Ok(events)
    }

    /// Process the original semicolon-delimited payload (without ESC/OSC/ST).
    pub fn push_payload(
        &mut self,
        payload: &str,
    ) -> Result<Option<OscNotificationEvent>, OscParseError> {
        if payload.len() > MAX_BRIDGED_OSC_BYTES {
            return Err(OscParseError::MarkerTooLarge);
        }

        let Some((code, fields)) = payload.split_once(';') else {
            return Err(OscParseError::MalformedSequence);
        };
        match code {
            "9" => parse_osc_9(fields).map(|notification| {
                notification
                    .map(|notification| OscNotificationEvent::Notification(Box::new(notification)))
            }),
            "99" => self.push_kitty(fields),
            "777" => parse_osc_777(fields).map(|notification| {
                notification
                    .map(|notification| OscNotificationEvent::Notification(Box::new(notification)))
            }),
            _ => Ok(None),
        }
    }

    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    pub fn clear_pending(&mut self) {
        self.pending.clear();
        self.pending_order.clear();
    }

    fn push_kitty(&mut self, fields: &str) -> Result<Option<OscNotificationEvent>, OscParseError> {
        let Some((metadata, payload)) = fields.split_once(';') else {
            return Err(OscParseError::MalformedSequence);
        };
        let frame = parse_kitty_metadata(metadata)?;

        match frame.payload_kind {
            KittyPayloadKind::Close => {
                let Some(identifier) = frame.identifier else {
                    return Ok(None);
                };
                self.discard_pending(&identifier);
                return Ok(Some(OscNotificationEvent::Close { identifier }));
            }
            KittyPayloadKind::Alive => {
                return Ok(Some(OscNotificationEvent::AliveQuery {
                    identifier: frame.identifier,
                }));
            }
            KittyPayloadKind::Capabilities => {
                return Ok(Some(OscNotificationEvent::CapabilityQuery {
                    identifier: frame.identifier,
                }));
            }
            KittyPayloadKind::Unknown => return Ok(None),
            _ => {}
        }

        if !frame.done && frame.identifier.is_none() {
            return Err(OscParseError::InvalidIdentifier);
        }

        let identifier = frame.identifier.clone();
        let mut pending = identifier
            .as_deref()
            .and_then(|identifier| self.take_pending(identifier))
            .unwrap_or_default();
        if pending.metadata.identifier.is_none() {
            pending.metadata.identifier.clone_from(&identifier);
        }

        if let Err(error) = pending.push_frame(&frame, payload) {
            if let Some(identifier) = identifier.as_deref() {
                self.discard_pending(identifier);
            }
            return Err(error);
        }

        if !frame.done {
            self.store_pending(
                identifier.expect("incomplete frame must have an identifier"),
                pending,
            );
            return Ok(None);
        }

        pending.finish().map(|notification| {
            notification
                .map(|notification| OscNotificationEvent::Notification(Box::new(notification)))
        })
    }

    fn take_pending(&mut self, identifier: &str) -> Option<PendingKittyNotification> {
        self.pending_order.retain(|entry| entry != identifier);
        self.pending.remove(identifier)
    }

    fn discard_pending(&mut self, identifier: &str) {
        self.pending_order.retain(|entry| entry != identifier);
        self.pending.remove(identifier);
    }

    fn store_pending(&mut self, identifier: String, pending: PendingKittyNotification) {
        while self.pending.len() >= MAX_PENDING_KITTY_NOTIFICATIONS {
            let Some(oldest) = self.pending_order.pop_front() else {
                self.pending.clear();
                break;
            };
            self.pending.remove(&oldest);
        }
        self.pending_order.push_back(identifier.clone());
        self.pending.insert(identifier, pending);
    }
}

/// Compare wrapping, non-zero bridge sequence numbers using serial-number
/// arithmetic. Distances of half the sequence space or more are stale.
pub(crate) fn bridge_sequence_is_after(candidate: u64, seen: u64) -> bool {
    let distance = candidate.wrapping_sub(seen);
    distance != 0 && distance < (1_u64 << 63)
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct BridgedOscFrame<'a> {
    pub payload: &'a str,
    /// Present on sequenced bridge frames. Legacy unit-level bridge strings do
    /// not have a sequence and are still accepted by the parser.
    pub sequence: Option<u64>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct BridgedOscEnvelope<'a> {
    pub frames: Vec<BridgedOscFrame<'a>>,
    /// `Some(None)` means reset the title; `Some(Some(_))` carries a title;
    /// `None` denotes a legacy frame without restoration metadata.
    pub restore_title: Option<Option<String>>,
    /// Replay envelopes are acknowledged through the PTY after every frame in
    /// the watermark has been processed. V2/legacy envelopes have no ACK.
    pub ack_sequence: Option<u64>,
}

/// Decode the sequenced bridge envelope emitted by vendored Alacritty.
pub(crate) fn decode_bridged_osc_title(
    title: &str,
) -> Result<Option<BridgedOscEnvelope<'_>>, OscParseError> {
    let Some(payload) = title.strip_prefix(OSC_NOTIFICATION_TITLE_PREFIX) else {
        return Ok(None);
    };

    if let Some(mut replay) = payload.strip_prefix("@3;") {
        if title.len() > MAX_BRIDGE_ENVELOPE_BYTES {
            return Err(OscParseError::MarkerTooLarge);
        }
        let latest = take_ascii_field(&mut replay, b';').and_then(parse_nonzero_sequence)?;
        let restore = take_ascii_field(&mut replay, b';')?;
        let restore_title = parse_restore_title(restore)?;
        let count = take_ascii_field(&mut replay, b';')?
            .parse::<usize>()
            .ok()
            .filter(|count| (1..=MAX_BRIDGE_REPLAY_ENTRIES).contains(count))
            .ok_or(OscParseError::MalformedSequence)?;

        let mut frames = Vec::with_capacity(count);
        let mut previous: Option<u64> = None;
        for _ in 0..count {
            let sequence = take_ascii_field(&mut replay, b',').and_then(parse_nonzero_sequence)?;
            if let Some(previous) = previous {
                let mut expected = previous.wrapping_add(1);
                if expected == 0 {
                    expected = 1;
                }
                if sequence != expected {
                    return Err(OscParseError::MalformedSequence);
                }
            }
            previous = Some(sequence);

            let payload_len = take_ascii_field(&mut replay, b':')?
                .parse::<usize>()
                .ok()
                .filter(|len| *len <= MAX_BRIDGED_OSC_BYTES)
                .ok_or(OscParseError::MalformedSequence)?;
            if replay.as_bytes().get(payload_len) != Some(&b';') {
                return Err(OscParseError::MalformedSequence);
            }
            let frame_payload = replay
                .get(..payload_len)
                .ok_or(OscParseError::MalformedSequence)?;
            replay = replay
                .get(payload_len + 1..)
                .ok_or(OscParseError::MalformedSequence)?;
            frames.push(BridgedOscFrame {
                payload: frame_payload,
                sequence: Some(sequence),
            });
        }
        if !replay.is_empty() || previous != Some(latest) {
            return Err(OscParseError::MalformedSequence);
        }
        return Ok(Some(BridgedOscEnvelope {
            frames,
            restore_title: Some(restore_title),
            ack_sequence: Some(latest),
        }));
    }

    let Some(envelope) = payload.strip_prefix("@2;") else {
        if payload.len() > MAX_BRIDGED_OSC_BYTES {
            return Err(OscParseError::MarkerTooLarge);
        }
        return Ok(Some(BridgedOscEnvelope {
            frames: vec![BridgedOscFrame {
                payload,
                sequence: None,
            }],
            restore_title: None,
            ack_sequence: None,
        }));
    };
    let (sequence, envelope) = envelope
        .split_once(';')
        .ok_or(OscParseError::MalformedSequence)?;
    let sequence = sequence
        .parse::<u64>()
        .ok()
        .filter(|sequence| *sequence != 0)
        .ok_or(OscParseError::MalformedSequence)?;
    let (restore, payload) = envelope
        .split_once(';')
        .ok_or(OscParseError::MalformedSequence)?;
    if payload.len() > MAX_BRIDGED_OSC_BYTES {
        return Err(OscParseError::MarkerTooLarge);
    }
    let restore_title = parse_restore_title(restore)?;
    Ok(Some(BridgedOscEnvelope {
        frames: vec![BridgedOscFrame {
            payload,
            sequence: Some(sequence),
        }],
        restore_title: Some(restore_title),
        ack_sequence: None,
    }))
}

fn take_ascii_field<'a>(input: &mut &'a str, delimiter: u8) -> Result<&'a str, OscParseError> {
    let index = input
        .as_bytes()
        .iter()
        .position(|byte| *byte == delimiter)
        .ok_or(OscParseError::MalformedSequence)?;
    let field = &input[..index];
    *input = &input[index + 1..];
    Ok(field)
}

fn parse_nonzero_sequence(value: &str) -> Result<u64, OscParseError> {
    value
        .parse::<u64>()
        .ok()
        .filter(|sequence| *sequence != 0)
        .ok_or(OscParseError::MalformedSequence)
}

fn parse_restore_title(restore: &str) -> Result<Option<String>, OscParseError> {
    match restore {
        "n" => Ok(None),
        restore if restore.starts_with('t') => {
            let bytes = decode_base64(&restore.as_bytes()[1..], MAX_BRIDGE_RESTORE_TITLE_BYTES)?;
            Ok(Some(
                String::from_utf8(bytes).map_err(|_| OscParseError::InvalidPayload)?,
            ))
        }
        _ => Err(OscParseError::MalformedSequence),
    }
}

/// Extract the raw payload from a reserved bridge title.
pub fn bridged_osc_payload(title: &str) -> Result<Option<&str>, OscParseError> {
    Ok(decode_bridged_osc_title(title)?
        .and_then(|envelope| envelope.frames.last().map(|frame| frame.payload)))
}

/// Convenience parser for a complete, single-frame notification.
/// Stateful Kitty chunking should use [`OscNotificationParser`] instead.
pub fn parse_osc_payload(payload: &[u8]) -> Option<OscNotification> {
    try_parse_osc_payload(payload).ok().flatten()
}

pub fn try_parse_osc_payload(payload: &[u8]) -> Result<Option<OscNotification>, OscParseError> {
    let payload = std::str::from_utf8(payload).map_err(|_| OscParseError::InvalidPayload)?;
    let mut parser = OscNotificationParser::new();
    match parser.push_payload(payload)? {
        Some(OscNotificationEvent::Notification(notification)) => Ok(Some(*notification)),
        _ => Ok(None),
    }
}

fn parse_osc_9(fields: &str) -> Result<Option<OscNotification>, OscParseError> {
    // iTerm2 also uses OSC 9;4 for progress bars; it is not a notification.
    if fields == "4" || fields.starts_with("4;") {
        return Ok(None);
    }
    validate_plain_payload(fields, MAX_BRIDGED_OSC_BYTES)?;
    if fields.is_empty() {
        return Ok(None);
    }

    Ok(Some(OscNotification {
        source: NotificationSource::Osc9,
        level: NotificationLevel::Info,
        title: "Terminal notification".to_string(),
        body: fields.to_string(),
        kitty: None,
    }))
}

fn parse_osc_777(fields: &str) -> Result<Option<OscNotification>, OscParseError> {
    let mut fields = fields.splitn(3, ';');
    if fields.next() != Some("notify") {
        return Ok(None);
    }
    let title = fields.next().ok_or(OscParseError::MalformedSequence)?;
    let body = fields.next().unwrap_or_default();
    validate_plain_payload(title, MAX_BRIDGED_OSC_BYTES)?;
    validate_plain_payload(body, MAX_BRIDGED_OSC_BYTES)?;

    let (title, body) = title_and_body(title.to_string(), body.to_string());
    if title.is_empty() {
        return Ok(None);
    }
    Ok(Some(OscNotification {
        source: NotificationSource::Osc777,
        level: NotificationLevel::Info,
        title,
        body,
        kitty: None,
    }))
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum KittyPayloadKind {
    #[default]
    Title,
    Body,
    Buttons,
    Icon,
    Close,
    Alive,
    Capabilities,
    Unknown,
}

#[derive(Default)]
struct KittyFrameMetadata {
    identifier: Option<String>,
    done: bool,
    base64: Option<bool>,
    payload_kind: KittyPayloadKind,
    application_name: Option<String>,
    notification_types: Vec<String>,
    urgency: Option<KittyUrgency>,
    delivery_condition: Option<KittyDeliveryCondition>,
    activation: Option<KittyActivation>,
    request_close_report: Option<bool>,
    sound_name: Option<String>,
    close_after_ms: Option<Option<u32>>,
    icon_identifier: Option<String>,
    icon_names: Vec<String>,
}

fn parse_kitty_metadata(metadata: &str) -> Result<KittyFrameMetadata, OscParseError> {
    let mut frame = KittyFrameMetadata {
        done: true,
        ..Default::default()
    };
    if metadata.is_empty() {
        return Ok(frame);
    }

    for pair in metadata.split(':') {
        let Some((key, value)) = pair.split_once('=') else {
            return Err(OscParseError::InvalidMetadata);
        };
        if key.len() != 1 || !key.as_bytes()[0].is_ascii_alphabetic() {
            return Err(OscParseError::InvalidMetadata);
        }
        validate_metadata_value(value)?;

        match key.as_bytes()[0] {
            b'a' => frame.activation = Some(parse_activation(value)?),
            b'c' => frame.request_close_report = Some(parse_bool(value)?),
            b'd' => frame.done = parse_bool(value)?,
            b'e' => frame.base64 = Some(parse_bool(value)?),
            b'f' => frame.application_name = Some(decode_metadata_text(value)?),
            b'g' => frame.icon_identifier = Some(parse_identifier(value)?),
            b'i' => frame.identifier = Some(parse_identifier(value)?),
            b'n' => push_metadata_item(&mut frame.icon_names, decode_metadata_text(value)?)?,
            b'o' => {
                frame.delivery_condition = Some(match value {
                    "always" => KittyDeliveryCondition::Always,
                    "unfocused" => KittyDeliveryCondition::Unfocused,
                    "invisible" => KittyDeliveryCondition::Invisible,
                    _ => return Err(OscParseError::InvalidMetadata),
                })
            }
            b'p' => {
                frame.payload_kind = match value {
                    "title" => KittyPayloadKind::Title,
                    "body" => KittyPayloadKind::Body,
                    "buttons" => KittyPayloadKind::Buttons,
                    "icon" => KittyPayloadKind::Icon,
                    "close" => KittyPayloadKind::Close,
                    "alive" => KittyPayloadKind::Alive,
                    "?" => KittyPayloadKind::Capabilities,
                    _ => KittyPayloadKind::Unknown,
                }
            }
            b's' => frame.sound_name = Some(decode_metadata_text(value)?),
            b't' => {
                push_metadata_item(&mut frame.notification_types, decode_metadata_text(value)?)?
            }
            b'u' => {
                frame.urgency = Some(match value {
                    "0" => KittyUrgency::Low,
                    "1" => KittyUrgency::Normal,
                    "2" => KittyUrgency::Critical,
                    _ => return Err(OscParseError::InvalidMetadata),
                })
            }
            b'w' => {
                let value = value
                    .parse::<i64>()
                    .map_err(|_| OscParseError::InvalidMetadata)?;
                frame.close_after_ms = Some(match value {
                    -1 => None,
                    value if (0..=i64::from(u32::MAX)).contains(&value) => Some(value as u32),
                    _ => return Err(OscParseError::InvalidMetadata),
                });
            }
            _ => {
                // Unknown keys are intentionally ignored for forward compatibility.
            }
        }
    }

    Ok(frame)
}

fn validate_metadata_value(value: &str) -> Result<(), OscParseError> {
    if value.len() > MAX_KITTY_METADATA_VALUE_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_/+.,(){}[]*&^%$#@!`~=?".contains(&byte))
    {
        return Err(OscParseError::InvalidMetadata);
    }
    Ok(())
}

fn parse_bool(value: &str) -> Result<bool, OscParseError> {
    match value {
        "0" => Ok(false),
        "1" => Ok(true),
        _ => Err(OscParseError::InvalidMetadata),
    }
}

fn parse_identifier(value: &str) -> Result<String, OscParseError> {
    if value.is_empty()
        || value.len() > MAX_KITTY_IDENTIFIER_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'+' | b'.'))
    {
        return Err(OscParseError::InvalidIdentifier);
    }
    Ok(value.to_string())
}

fn parse_activation(value: &str) -> Result<KittyActivation, OscParseError> {
    let mut activation = KittyActivation::default();
    if value.is_empty() {
        return Err(OscParseError::InvalidMetadata);
    }
    for action in value.split(',') {
        match action {
            "focus" => activation.focus = true,
            "-focus" => activation.focus = false,
            "report" => activation.report = true,
            "-report" => activation.report = false,
            // Unknown actions are ignored as required for protocol evolution.
            _ => {}
        }
    }
    Ok(activation)
}

fn decode_metadata_text(value: &str) -> Result<String, OscParseError> {
    let decoded = decode_base64(value.as_bytes(), MAX_KITTY_METADATA_TEXT_BYTES)?;
    decoded_text(decoded)
}

fn push_metadata_item(items: &mut Vec<String>, item: String) -> Result<(), OscParseError> {
    if items.len() >= MAX_KITTY_METADATA_ITEMS {
        return Err(OscParseError::InvalidMetadata);
    }
    items.push(item);
    Ok(())
}

#[derive(Clone, Default)]
struct PendingKittyNotification {
    metadata: KittyNotificationMetadata,
    title: Vec<PayloadChunk>,
    body: Vec<PayloadChunk>,
    buttons: Vec<PayloadChunk>,
    raw_payload_bytes: usize,
    frame_count: usize,
}

impl PendingKittyNotification {
    fn push_frame(
        &mut self,
        frame: &KittyFrameMetadata,
        payload: &str,
    ) -> Result<(), OscParseError> {
        self.frame_count = self
            .frame_count
            .checked_add(1)
            .filter(|count| *count <= MAX_KITTY_NOTIFICATION_FRAMES)
            .ok_or(OscParseError::NotificationTooLarge)?;
        self.apply_metadata(frame)?;
        // `e` applies to this payload frame only and defaults to zero on the
        // next frame, even when both frames share a Kitty identifier.
        let base64 = frame.base64.unwrap_or(false);
        validate_chunk(payload, base64)?;
        self.raw_payload_bytes = self
            .raw_payload_bytes
            .checked_add(payload.len())
            .filter(|bytes| *bytes <= MAX_OSC_NOTIFICATION_BYTES)
            .ok_or(OscParseError::NotificationTooLarge)?;

        let chunk = PayloadChunk {
            base64,
            bytes: payload.as_bytes().to_vec(),
        };
        match frame.payload_kind {
            KittyPayloadKind::Title => self.title.push(chunk),
            KittyPayloadKind::Body => self.body.push(chunk),
            KittyPayloadKind::Buttons => self.buttons.push(chunk),
            // Icon data is deliberately not retained until zmux has an icon
            // presentation surface. It still counts toward the aggregate bound.
            KittyPayloadKind::Icon => {}
            _ => {}
        }
        Ok(())
    }

    fn apply_metadata(&mut self, frame: &KittyFrameMetadata) -> Result<(), OscParseError> {
        if let Some(application_name) = &frame.application_name {
            self.metadata.application_name = Some(application_name.clone());
        }
        extend_metadata_items(
            &mut self.metadata.notification_types,
            &frame.notification_types,
        )?;
        if let Some(urgency) = frame.urgency {
            self.metadata.urgency = urgency;
        }
        if let Some(delivery_condition) = frame.delivery_condition {
            self.metadata.delivery_condition = delivery_condition;
        }
        if let Some(activation) = frame.activation {
            self.metadata.activation = activation;
        }
        if let Some(request_close_report) = frame.request_close_report {
            self.metadata.request_close_report = request_close_report;
        }
        if let Some(sound_name) = &frame.sound_name {
            self.metadata.sound_name = Some(sound_name.clone());
        }
        if let Some(close_after_ms) = frame.close_after_ms {
            self.metadata.close_after_ms = close_after_ms;
        }
        if let Some(icon_identifier) = &frame.icon_identifier {
            self.metadata.icon_identifier = Some(icon_identifier.clone());
        }
        extend_metadata_items(&mut self.metadata.icon_names, &frame.icon_names)?;
        Ok(())
    }

    fn finish(mut self) -> Result<Option<OscNotification>, OscParseError> {
        let title = decode_payload_chunks(&self.title)?;
        let body = decode_payload_chunks(&self.body)?;
        let buttons = decode_payload_chunks(&self.buttons)?;

        if !buttons.is_empty() {
            for button in buttons.split('\u{2028}') {
                if button.is_empty() {
                    continue;
                }
                if self.metadata.buttons.len() >= MAX_KITTY_METADATA_ITEMS {
                    return Err(OscParseError::InvalidPayload);
                }
                self.metadata.buttons.push(button.to_string());
            }
        }

        let (title, body) = title_and_body(title, body);
        if title.is_empty() {
            return Ok(None);
        }
        let decoded_size = title
            .len()
            .checked_add(body.len())
            .ok_or(OscParseError::NotificationTooLarge)?;
        if decoded_size > MAX_OSC_NOTIFICATION_BYTES {
            return Err(OscParseError::NotificationTooLarge);
        }

        let level = match self.metadata.urgency {
            KittyUrgency::Low | KittyUrgency::Normal => NotificationLevel::Info,
            KittyUrgency::Critical => NotificationLevel::Error,
        };
        Ok(Some(OscNotification {
            source: NotificationSource::Osc99,
            level,
            title,
            body,
            kitty: Some(self.metadata),
        }))
    }
}

fn extend_metadata_items(
    destination: &mut Vec<String>,
    source: &[String],
) -> Result<(), OscParseError> {
    if destination.len().saturating_add(source.len()) > MAX_KITTY_METADATA_ITEMS {
        return Err(OscParseError::InvalidMetadata);
    }
    destination.extend_from_slice(source);
    Ok(())
}

#[derive(Clone)]
struct PayloadChunk {
    base64: bool,
    bytes: Vec<u8>,
}

fn validate_chunk(payload: &str, base64: bool) -> Result<(), OscParseError> {
    let limit = if base64 {
        MAX_KITTY_ENCODED_CHUNK_BYTES
    } else {
        MAX_KITTY_PLAIN_CHUNK_BYTES
    };
    if payload.len() > limit {
        return Err(OscParseError::PayloadTooLarge);
    }
    if base64 {
        validate_base64_fragment(payload.as_bytes())
    } else {
        validate_plain_payload(payload, limit)
    }
}

fn validate_plain_payload(payload: &str, limit: usize) -> Result<(), OscParseError> {
    if payload.len() > limit {
        return Err(OscParseError::PayloadTooLarge);
    }
    if payload
        .chars()
        .any(|character| matches!(character as u32, 0x00..=0x1f | 0x7f..=0x9f))
    {
        return Err(OscParseError::InvalidPayload);
    }
    Ok(())
}

fn validate_base64_fragment(payload: &[u8]) -> Result<(), OscParseError> {
    let mut padding = false;
    let mut padding_count = 0;
    for &byte in payload {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'+' | b'/' if !padding => {}
            b'=' => {
                padding = true;
                padding_count += 1;
                if padding_count > 2 {
                    return Err(OscParseError::InvalidBase64);
                }
            }
            _ => return Err(OscParseError::InvalidBase64),
        }
    }
    Ok(())
}

fn decode_payload_chunks(chunks: &[PayloadChunk]) -> Result<String, OscParseError> {
    let mut decoded = Vec::new();
    let mut index = 0;
    while index < chunks.len() {
        if !chunks[index].base64 {
            append_bounded(&mut decoded, &chunks[index].bytes)?;
            index += 1;
            continue;
        }

        let start = index;
        let mut encoded = Vec::new();
        while index < chunks.len() && chunks[index].base64 {
            append_bounded(&mut encoded, &chunks[index].bytes)?;
            index += 1;
        }

        match decode_base64(&encoded, MAX_OSC_NOTIFICATION_BYTES) {
            Ok(value) => append_bounded(&mut decoded, &value)?,
            Err(_) => {
                // Kitty allows chunking either before or after Base64. Internal
                // padding means each chunk was encoded independently.
                for chunk in &chunks[start..index] {
                    let value = decode_base64(&chunk.bytes, MAX_OSC_NOTIFICATION_BYTES)?;
                    append_bounded(&mut decoded, &value)?;
                }
            }
        }
    }
    decoded_text(decoded)
}

fn append_bounded(destination: &mut Vec<u8>, source: &[u8]) -> Result<(), OscParseError> {
    let new_len = destination
        .len()
        .checked_add(source.len())
        .filter(|len| *len <= MAX_OSC_NOTIFICATION_BYTES)
        .ok_or(OscParseError::NotificationTooLarge)?;
    destination.reserve(new_len - destination.len());
    destination.extend_from_slice(source);
    Ok(())
}

fn decode_base64(input: &[u8], max_output: usize) -> Result<Vec<u8>, OscParseError> {
    validate_base64_fragment(input)?;
    let padding = input.iter().rev().take_while(|byte| **byte == b'=').count();
    let meaningful_len = input.len().saturating_sub(padding);
    let remainder = meaningful_len % 4;
    if remainder == 1
        || (padding > 0 && !input.len().is_multiple_of(4))
        || (padding == 1 && remainder != 3)
        || (padding == 2 && remainder != 2)
    {
        return Err(OscParseError::InvalidBase64);
    }

    let expected_len = meaningful_len
        .checked_mul(6)
        .map(|bits| bits / 8)
        .filter(|len| *len <= max_output)
        .ok_or(OscParseError::PayloadTooLarge)?;
    let mut output = Vec::with_capacity(expected_len);
    let mut accumulator = 0u32;
    let mut bits = 0u8;
    for &byte in &input[..meaningful_len] {
        let value = base64_value(byte).ok_or(OscParseError::InvalidBase64)?;
        accumulator = (accumulator << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            output.push((accumulator >> bits) as u8);
            accumulator &= (1u32 << bits) - 1;
        }
    }
    if accumulator != 0 || output.len() != expected_len {
        return Err(OscParseError::InvalidBase64);
    }
    Ok(output)
}

fn base64_value(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

fn decoded_text(bytes: Vec<u8>) -> Result<String, OscParseError> {
    let text = String::from_utf8(bytes).map_err(|_| OscParseError::InvalidPayload)?;
    if text
        .chars()
        .any(|character| matches!(character as u32, 0x00 | 0x1b | 0x7f..=0x9f))
    {
        return Err(OscParseError::InvalidPayload);
    }
    Ok(text)
}

fn title_and_body(mut title: String, mut body: String) -> (String, String) {
    if title.is_empty() && !body.is_empty() {
        title = std::mem::take(&mut body);
    }
    (title, body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notification(event: Option<OscNotificationEvent>) -> OscNotification {
        match event.expect("expected an OSC event") {
            OscNotificationEvent::Notification(notification) => *notification,
            event => panic!("expected a notification, got {event:?}"),
        }
    }

    #[test]
    fn bridge_ignores_real_titles_and_parses_osc_9() {
        let mut parser = OscNotificationParser::new();
        assert_eq!(parser.push_title("real title"), Ok(Vec::new()));

        let title = format!("{OSC_NOTIFICATION_TITLE_PREFIX}9;build complete");
        let notification = notification(parser.push_title(&title).unwrap().pop());
        assert_eq!(notification.source, NotificationSource::Osc9);
        assert_eq!(notification.title, "Terminal notification");
        assert_eq!(notification.body, "build complete");
    }

    #[test]
    fn sequenced_bridge_decodes_payload_and_title_restoration() {
        let title = format!("{OSC_NOTIFICATION_TITLE_PREFIX}@2;7;td29ya3NwYWNl;99;i=build;done");
        let envelope = decode_bridged_osc_title(&title).unwrap().unwrap();
        assert_eq!(envelope.ack_sequence, None);
        assert_eq!(envelope.restore_title, Some(Some("workspace".to_owned())));
        assert_eq!(envelope.frames[0].sequence, Some(7));
        assert_eq!(envelope.frames[0].payload, "99;i=build;done");
        assert_eq!(
            bridged_osc_payload(&title).unwrap(),
            Some(envelope.frames[0].payload)
        );

        let reset = format!("{OSC_NOTIFICATION_TITLE_PREFIX}@2;8;n;9;hello");
        let envelope = decode_bridged_osc_title(&reset).unwrap().unwrap();
        assert_eq!(envelope.frames[0].sequence, Some(8));
        assert_eq!(envelope.restore_title, Some(None));
        assert_eq!(envelope.frames[0].payload, "9;hello");
    }

    #[test]
    fn replay_bridge_decodes_every_sequenced_payload() {
        let title = format!(
            "{OSC_NOTIFICATION_TITLE_PREFIX}@3;9;td29ya3NwYWNl;3;7,5:9;one;8,6:9;two!;9,21:777;notify;Build;done;"
        );
        let envelope = decode_bridged_osc_title(&title).unwrap().unwrap();

        assert_eq!(envelope.ack_sequence, Some(9));
        assert_eq!(envelope.restore_title, Some(Some("workspace".to_owned())));
        assert_eq!(
            envelope
                .frames
                .iter()
                .map(|frame| (frame.sequence, frame.payload))
                .collect::<Vec<_>>(),
            [
                (Some(7), "9;one"),
                (Some(8), "9;two!"),
                (Some(9), "777;notify;Build;done"),
            ]
        );

        let mut parser = OscNotificationParser::new();
        let events = parser.push_title(&title).unwrap();
        assert_eq!(events.len(), 3);
    }

    #[test]
    fn public_title_parser_suppresses_duplicate_and_stale_replay_frames() {
        let replay =
            format!("{OSC_NOTIFICATION_TITLE_PREFIX}@3;9;n;3;7,5:9;one;8,6:9;two!;9,7:9;three;");
        let stale = format!("{OSC_NOTIFICATION_TITLE_PREFIX}@2;8;n;9;stale");
        let mut parser = OscNotificationParser::new();

        assert_eq!(parser.push_title(&replay).unwrap().len(), 3);
        assert!(parser.push_title(&replay).unwrap().is_empty());
        assert!(parser.push_title(&stale).unwrap().is_empty());
    }

    #[test]
    fn public_title_parser_accepts_only_the_new_tail_of_a_replay() {
        let first = format!("{OSC_NOTIFICATION_TITLE_PREFIX}@3;8;n;2;7,5:9;one;8,6:9;two!;");
        let extended = format!(
            "{OSC_NOTIFICATION_TITLE_PREFIX}@3;10;n;4;7,5:9;one;8,6:9;two!;9,7:9;three;10,6:9;four;"
        );
        let mut parser = OscNotificationParser::new();

        assert_eq!(parser.push_title(&first).unwrap().len(), 2);
        let events = parser.push_title(&extended).unwrap();
        assert_eq!(events.len(), 2);
        let bodies = events
            .into_iter()
            .map(|event| notification(Some(event)).body)
            .collect::<Vec<_>>();
        assert_eq!(bodies, ["three", "four"]);
    }

    #[test]
    fn public_title_parser_orders_bridge_sequences_across_wraparound() {
        let before_wrap = format!(
            "{OSC_NOTIFICATION_TITLE_PREFIX}@2;{};n;9;before wrap",
            u64::MAX
        );
        let after_wrap = format!("{OSC_NOTIFICATION_TITLE_PREFIX}@2;1;n;9;after wrap");
        let now_stale = format!("{OSC_NOTIFICATION_TITLE_PREFIX}@2;{};n;9;stale", u64::MAX);
        let mut parser = OscNotificationParser::new();

        assert_eq!(parser.push_title(&before_wrap).unwrap().len(), 1);
        assert_eq!(parser.push_title(&after_wrap).unwrap().len(), 1);
        assert!(parser.push_title(&now_stale).unwrap().is_empty());
        assert!(bridge_sequence_is_after(1, u64::MAX));
        assert!(!bridge_sequence_is_after(u64::MAX, 1));
    }

    #[test]
    fn public_title_parser_rolls_back_a_partially_malformed_replay_for_retry() {
        let malformed = format!("{OSC_NOTIFICATION_TITLE_PREFIX}@3;2;n;2;1,5:9;one;2,6:99;bad;");
        let corrected = format!("{OSC_NOTIFICATION_TITLE_PREFIX}@3;2;n;2;1,5:9;one;2,5:9;two;");
        let mut parser = OscNotificationParser::new();

        assert_eq!(
            parser.push_title(&malformed),
            Err(OscParseError::MalformedSequence)
        );
        assert_eq!(parser.bridge_sequence_watermark, None);
        assert_eq!(parser.pending_len(), 0);

        let events = parser.push_title(&corrected).unwrap();
        let bodies = events
            .into_iter()
            .map(|event| notification(Some(event)).body)
            .collect::<Vec<_>>();
        assert_eq!(bodies, ["one", "two"]);
        assert_eq!(parser.bridge_sequence_watermark, Some(2));
    }

    #[test]
    fn replay_bridge_rejects_bad_lengths_counts_and_watermarks() {
        for suffix in [
            "@3;2;n;1;1,5:9;one;",
            "@3;1;n;2;1,5:9;one;",
            "@3;1;n;1;1,6:9;one;",
            "@3;2;n;2;1,5:9;one;3,5:9;two;",
            "@3;1;n;1;1,5:9;one",
        ] {
            let title = format!("{OSC_NOTIFICATION_TITLE_PREFIX}{suffix}");
            assert!(decode_bridged_osc_title(&title).is_err(), "{suffix}");
        }
    }

    #[test]
    fn sequenced_bridge_rejects_invalid_metadata() {
        for suffix in [
            "@2;0;n;9;hello",
            "@2;wat;n;9;hello",
            "@2;1;x;9;hello",
            "@2;1;t!!!!;9;hello",
        ] {
            let title = format!("{OSC_NOTIFICATION_TITLE_PREFIX}{suffix}");
            assert!(decode_bridged_osc_title(&title).is_err(), "{suffix}");
        }
    }

    #[test]
    fn osc_9_progress_is_not_misreported_as_a_notification() {
        assert_eq!(parse_osc_payload(b"9;4;1;75"), None);
    }

    #[test]
    fn parses_osc_777_and_preserves_semicolons_in_the_body() {
        let notification = parse_osc_payload(b"777;notify;Build;tests; passed").unwrap();
        assert_eq!(notification.source, NotificationSource::Osc777);
        assert_eq!(notification.title, "Build");
        assert_eq!(notification.body, "tests; passed");
    }

    #[test]
    fn assembles_kitty_title_and_body_chunks() {
        let mut parser = OscNotificationParser::new();
        assert_eq!(parser.push_payload("99;i=build:d=0;Build").unwrap(), None);
        let result = parser
            .push_payload("99;i=build:p=body:d=1;tests passed")
            .unwrap();
        let notification = notification(result);
        assert_eq!(notification.title, "Build");
        assert_eq!(notification.body, "tests passed");
        assert_eq!(
            notification.kitty.unwrap().identifier.as_deref(),
            Some("build")
        );
        assert_eq!(parser.pending_len(), 0);
    }

    #[test]
    fn decodes_base64_split_after_encoding_without_padding() {
        let mut parser = OscNotificationParser::new();
        parser.push_payload("99;i=greeting:e=1:d=0;SGVs").unwrap();
        let notification = notification(parser.push_payload("99;i=greeting:e=1:d=1;bG8").unwrap());
        assert_eq!(notification.title, "Hello");
    }

    #[test]
    fn base64_encoding_flag_is_scoped_to_one_payload_frame() {
        let mut parser = OscNotificationParser::new();
        assert_eq!(
            parser.push_payload("99;i=build:e=1:d=0;VGl0bGU=").unwrap(),
            None
        );
        let notification = notification(
            parser
                .push_payload("99;i=build:p=body:d=1;plain body")
                .unwrap(),
        );
        assert_eq!(notification.title, "Title");
        assert_eq!(notification.body, "plain body");
    }

    #[test]
    fn decodes_chunks_that_were_base64_encoded_independently() {
        let mut parser = OscNotificationParser::new();
        parser.push_payload("99;i=greeting:e=1:d=0;SGk=").unwrap();
        let notification = notification(parser.push_payload("99;i=greeting:e=1:d=1;IQ==").unwrap());
        assert_eq!(notification.title, "Hi!");
    }

    #[test]
    fn parses_kitty_presentation_metadata_and_buttons() {
        let mut parser = OscNotificationParser::new();
        parser
            .push_payload(concat!(
                "99;i=job:f=Y29kZXg=:t=YnVpbGQ=:u=2:o=invisible:",
                "a=report,-focus:c=1:w=500:s=c2lsZW50:n=dGVybWluYWw=:d=0;Build"
            ))
            .unwrap();
        parser.push_payload("99;i=job:p=body:d=0;failed").unwrap();
        let notification = notification(
            parser
                .push_payload("99;i=job:p=buttons:d=1;Retry\u{2028}Dismiss")
                .unwrap(),
        );

        assert_eq!(notification.level, NotificationLevel::Error);
        let metadata = notification.kitty.unwrap();
        assert_eq!(metadata.application_name.as_deref(), Some("codex"));
        assert_eq!(metadata.notification_types, ["build"]);
        assert_eq!(metadata.urgency, KittyUrgency::Critical);
        assert_eq!(
            metadata.delivery_condition,
            KittyDeliveryCondition::Invisible
        );
        assert_eq!(
            metadata.activation,
            KittyActivation {
                focus: false,
                report: true
            }
        );
        assert!(metadata.request_close_report);
        assert_eq!(metadata.close_after_ms, Some(500));
        assert_eq!(metadata.sound_name.as_deref(), Some("silent"));
        assert_eq!(metadata.icon_names, ["terminal"]);
        assert_eq!(metadata.buttons, ["Retry", "Dismiss"]);
    }

    #[test]
    fn body_becomes_title_when_no_title_was_supplied() {
        let notification = parse_osc_payload(b"99;p=body;body only").unwrap();
        assert_eq!(notification.title, "body only");
        assert!(notification.body.is_empty());
    }

    #[test]
    fn close_event_discards_incomplete_state() {
        let mut parser = OscNotificationParser::new();
        assert_eq!(parser.push_payload("99;p=close;").unwrap(), None);
        parser.push_payload("99;i=job:d=0;Build").unwrap();
        assert_eq!(parser.pending_len(), 1);
        assert_eq!(
            parser.push_payload("99;i=job:p=close;").unwrap(),
            Some(OscNotificationEvent::Close {
                identifier: "job".to_string()
            })
        );
        assert_eq!(parser.pending_len(), 0);
    }

    #[test]
    fn incomplete_notifications_require_safe_identifiers() {
        let mut parser = OscNotificationParser::new();
        assert_eq!(
            parser.push_payload("99;d=0;title"),
            Err(OscParseError::InvalidIdentifier)
        );
        assert_eq!(
            parser.push_payload("99;i=bad/id:d=0;title"),
            Err(OscParseError::InvalidIdentifier)
        );
    }

    #[test]
    fn payload_and_base64_bounds_are_enforced() {
        let too_large = "x".repeat(MAX_KITTY_PLAIN_CHUNK_BYTES + 1);
        let payload = format!("99;;{too_large}");
        assert_eq!(
            try_parse_osc_payload(payload.as_bytes()),
            Err(OscParseError::PayloadTooLarge)
        );
        assert_eq!(
            try_parse_osc_payload(b"99;e=1;!!!!"),
            Err(OscParseError::InvalidBase64)
        );
        assert_eq!(
            try_parse_osc_payload(b"99;e=1;Zh=="),
            Err(OscParseError::InvalidBase64),
            "non-canonical trailing bits must be rejected"
        );
    }

    #[test]
    fn pending_kitty_state_is_bounded_and_evicts_oldest() {
        let mut parser = OscNotificationParser::new();
        for index in 0..=MAX_PENDING_KITTY_NOTIFICATIONS {
            parser
                .push_payload(&format!("99;i=id{index}:d=0;title"))
                .unwrap();
        }
        assert_eq!(parser.pending_len(), MAX_PENDING_KITTY_NOTIFICATIONS);

        let notification = notification(parser.push_payload("99;i=id0:d=1;done").unwrap());
        assert_eq!(notification.title, "done", "the oldest partial was evicted");
    }

    #[test]
    fn kitty_empty_chunks_can_finish_at_the_frame_limit() {
        let mut parser = OscNotificationParser::new();
        for _ in 0..MAX_KITTY_NOTIFICATION_FRAMES - 1 {
            assert_eq!(parser.push_payload("99;i=empty:d=0;").unwrap(), None);
        }

        let notification = notification(parser.push_payload("99;i=empty:d=1;complete").unwrap());
        assert_eq!(notification.title, "complete");
        assert_eq!(parser.pending_len(), 0);
    }

    #[test]
    fn kitty_frame_limit_rejects_empty_chunk_flood_and_clears_only_its_state() {
        let mut parser = OscNotificationParser::new();
        parser.push_payload("99;i=survivor:d=0;kept").unwrap();
        parser.push_payload("99;i=flood:d=0;stale").unwrap();
        for _ in 1..MAX_KITTY_NOTIFICATION_FRAMES {
            assert_eq!(parser.push_payload("99;i=flood:d=0;").unwrap(), None);
        }

        assert_eq!(parser.pending_len(), 2);
        let flood = parser.pending.get("flood").expect("flood state is pending");
        assert_eq!(flood.frame_count, MAX_KITTY_NOTIFICATION_FRAMES);
        assert_eq!(flood.title.len(), MAX_KITTY_NOTIFICATION_FRAMES);

        assert_eq!(
            parser.push_payload("99;i=flood:d=0;"),
            Err(OscParseError::NotificationTooLarge)
        );
        assert_eq!(parser.pending_len(), 1);
        assert!(!parser.pending.contains_key("flood"));
        assert!(parser.pending.contains_key("survivor"));

        let fresh = notification(parser.push_payload("99;i=flood:d=1;fresh").unwrap());
        assert_eq!(
            fresh.title, "fresh",
            "reused IDs must not recover stale chunks"
        );

        let survivor = notification(
            parser
                .push_payload("99;i=survivor:p=body:d=1;body")
                .unwrap(),
        );
        assert_eq!(survivor.title, "kept");
        assert_eq!(survivor.body, "body");
        assert_eq!(parser.pending_len(), 0);
    }

    #[test]
    fn malformed_or_oversized_markers_are_rejected() {
        let marker = format!(
            "{OSC_NOTIFICATION_TITLE_PREFIX}{}",
            "x".repeat(MAX_BRIDGED_OSC_BYTES + 1)
        );
        assert_eq!(
            bridged_osc_payload(&marker),
            Err(OscParseError::MarkerTooLarge)
        );
        assert_eq!(
            try_parse_osc_payload(b"99;missing-second-semicolon"),
            Err(OscParseError::MalformedSequence)
        );
    }
}
