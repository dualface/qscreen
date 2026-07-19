use base64::{Engine, engine::general_purpose::STANDARD as B64};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// 与 Go wire.go 保持字节兼容的常量
pub const MAX_CONTROL_MESSAGE_SIZE: usize = 64 * 1024;
pub const MAX_PAYLOAD_SIZE: usize = 64 * 1024;
pub const MAX_WIRE_MESSAGE_SIZE: usize =
    MAX_CONTROL_MESSAGE_SIZE + 4 * MAX_PAYLOAD_SIZE.div_ceil(3);
pub const MAX_FRAME_MESSAGE_SIZE: usize = 16 * 1024 * 1024;

pub const MIN_TERMINAL_WIDTH: u32 = 1;
pub const MAX_TERMINAL_WIDTH: u32 = 1000;
pub const MIN_TERMINAL_HEIGHT: u32 = 1;
pub const MAX_TERMINAL_HEIGHT: u32 = 500;

pub const DEFAULT_WIDTH: u32 = 80;
pub const DEFAULT_HEIGHT: u32 = 24;

// ── 枚举类型 ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum MessageKind {
    #[default]
    Request,
    Response,
    Event,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Command {
    New,
    List,
    Attach,
    Detach,
    Focus,
    Rename,
    Input,
    Resize,
    Kill,
    Stop,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EventType {
    Output,
    Frame,
    Exit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum AttachMode {
    #[default]
    Frame,
    Bytes,
}

pub const FRAME_FLAG_DIM: u8 = 0b0000_0001;
pub const FRAME_FLAG_BOLD: u8 = 0b0000_0010;
pub const FRAME_FLAG_ITALIC: u8 = 0b0000_0100;
pub const FRAME_FLAG_UNDERLINE: u8 = 0b0000_1000;
pub const FRAME_FLAG_INVERSE: u8 = 0b0001_0000;
pub const FRAME_FLAG_BLINK: u8 = 0b0010_0000;
pub const FRAME_FLAG_HIDDEN: u8 = 0b0100_0000;
pub const FRAME_FLAG_STRIKETHROUGH: u8 = 0b1000_0000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum FrameMouseMode {
    #[default]
    None,
    Press,
    PressRelease,
    ButtonMotion,
    AnyMotion,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum FrameMouseEncoding {
    #[default]
    Default,
    Utf8,
    Sgr,
}

/// Structured visible terminal state, copied from psmux's row/run model.
/// This avoids replaying a vt100 ANSI dump into an xterm host on reattach.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ScreenFrame {
    pub rows: u16,
    pub cols: u16,
    pub cursor_row: u16,
    pub cursor_col: u16,
    pub hide_cursor: bool,
    pub alternate_screen: bool,
    #[serde(skip_serializing_if = "is_zero_u8", default)]
    pub cursor_shape: u8,
    #[serde(skip_serializing_if = "is_false", default)]
    pub application_cursor: bool,
    #[serde(skip_serializing_if = "is_false", default)]
    pub bracketed_paste: bool,
    #[serde(skip_serializing_if = "is_frame_mouse_mode_none", default)]
    pub mouse_mode: FrameMouseMode,
    #[serde(skip_serializing_if = "is_frame_mouse_encoding_default", default)]
    pub mouse_encoding: FrameMouseEncoding,
    pub rows_v2: Vec<Vec<ScreenRun>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenRun {
    pub text: String,
    pub fg: FrameColor,
    pub bg: FrameColor,
    pub flags: u8,
    pub width: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "kind", content = "value", rename_all = "lowercase")]
pub enum FrameColor {
    #[default]
    Default,
    Idx(u8),
    Rgb(u8, u8, u8),
}

// ── SessionInfo ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SessionInfo {
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub session_id: String,
    pub name: String,
    pub attached: bool,
    pub exited: bool,
    #[serde(skip_serializing_if = "is_zero_i64", default)]
    pub exit_code: i64,
    pub created_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "is_zero_u32", default)]
    pub width: u32,
    #[serde(skip_serializing_if = "is_zero_u32", default)]
    pub height: u32,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub size: String,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub cwd: String,
}

// ── Message (公开 API) ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct Message {
    pub kind: MessageKind,
    pub id: String,
    pub command: Option<Command>,
    pub event: Option<EventType>,
    pub session_id: String,
    pub name: String,
    pub shell: String,
    pub cwd: String,
    pub width: u32,
    pub height: u32,
    pub ok: bool,
    pub error: String,
    pub sessions: Vec<SessionInfo>,
    pub exit_code: i64,
    pub payload: Vec<u8>,
    pub frame: Option<ScreenFrame>,
    pub attach_mode: AttachMode,
}

// ── Wire 格式 (JSON 序列化用) ────────────────────────────────────────────────

#[derive(Serialize, Deserialize)]
struct WireMessage {
    kind: MessageKind,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    id: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    command: Option<Command>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    event: Option<EventType>,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    session_id: String,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    name: String,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    shell: String,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    cwd: String,
    #[serde(skip_serializing_if = "is_zero_u32", default)]
    width: u32,
    #[serde(skip_serializing_if = "is_zero_u32", default)]
    height: u32,
    #[serde(skip_serializing_if = "is_false", default)]
    ok: bool,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    error: String,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    sessions: Vec<SessionInfo>,
    #[serde(skip_serializing_if = "is_zero_i64", default)]
    exit_code: i64,
    #[serde(
        rename = "payload_b64",
        skip_serializing_if = "String::is_empty",
        default
    )]
    payload_b64: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    frame: Option<ScreenFrame>,
    #[serde(skip_serializing_if = "is_attach_mode_frame", default)]
    attach_mode: AttachMode,
}

fn is_false(v: &bool) -> bool {
    !v
}
fn is_zero_u32(v: &u32) -> bool {
    *v == 0
}
fn is_zero_u8(v: &u8) -> bool {
    *v == 0
}
fn is_zero_i64(v: &i64) -> bool {
    *v == 0
}
fn is_attach_mode_frame(v: &AttachMode) -> bool {
    *v == AttachMode::Frame
}
fn is_frame_mouse_mode_none(v: &FrameMouseMode) -> bool {
    *v == FrameMouseMode::None
}
fn is_frame_mouse_encoding_default(v: &FrameMouseEncoding) -> bool {
    *v == FrameMouseEncoding::Default
}

// ── 编解码 ────────────────────────────────────────────────────────────────

impl Message {
    /// 序列化为 JSON line（末尾加 \n）
    pub fn to_json_line(&self) -> anyhow::Result<Vec<u8>> {
        if self.payload.len() > MAX_PAYLOAD_SIZE {
            anyhow::bail!(
                "payload too large: {} > {}",
                self.payload.len(),
                MAX_PAYLOAD_SIZE
            );
        }
        let wire = WireMessage {
            kind: self.kind.clone(),
            id: self.id.clone(),
            command: self.command.clone(),
            event: self.event.clone(),
            session_id: self.session_id.clone(),
            name: self.name.clone(),
            shell: self.shell.clone(),
            cwd: self.cwd.clone(),
            width: self.width,
            height: self.height,
            ok: self.ok,
            error: self.error.clone(),
            sessions: self.sessions.clone(),
            exit_code: self.exit_code,
            payload_b64: if self.payload.is_empty() {
                String::new()
            } else {
                B64.encode(&self.payload)
            },
            frame: self.frame.clone(),
            attach_mode: self.attach_mode,
        };
        let mut bytes = serde_json::to_vec(&wire)?;
        let limit = if self.frame.is_some() {
            MAX_FRAME_MESSAGE_SIZE
        } else {
            MAX_WIRE_MESSAGE_SIZE
        };
        if bytes.len() > limit {
            anyhow::bail!("message too large: {} > {}", bytes.len(), limit);
        }
        bytes.push(b'\n');
        Ok(bytes)
    }

    /// 从 JSON 字符串（含可选尾部空白）解析
    pub fn from_json(s: &str) -> anyhow::Result<Self> {
        let s = s.trim();
        if s.len() > MAX_FRAME_MESSAGE_SIZE {
            anyhow::bail!(
                "message too large: {} > {}",
                s.len(),
                MAX_FRAME_MESSAGE_SIZE
            );
        }
        let wire: WireMessage = serde_json::from_str(s)?;
        let limit = if wire.frame.is_some() {
            MAX_FRAME_MESSAGE_SIZE
        } else {
            MAX_WIRE_MESSAGE_SIZE
        };
        if s.len() > limit {
            anyhow::bail!("message too large: {} > {}", s.len(), limit);
        }
        let payload = if wire.payload_b64.is_empty() {
            Vec::new()
        } else {
            let decoded = B64.decode(&wire.payload_b64)?;
            if decoded.len() > MAX_PAYLOAD_SIZE {
                anyhow::bail!(
                    "payload too large after decode: {} > {}",
                    decoded.len(),
                    MAX_PAYLOAD_SIZE
                );
            }
            decoded
        };
        Ok(Message {
            kind: wire.kind,
            id: wire.id,
            command: wire.command,
            event: wire.event,
            session_id: wire.session_id,
            name: wire.name,
            shell: wire.shell,
            cwd: wire.cwd,
            width: wire.width,
            height: wire.height,
            ok: wire.ok,
            error: wire.error,
            sessions: wire.sessions,
            exit_code: wire.exit_code,
            payload,
            frame: wire.frame,
            attach_mode: wire.attach_mode,
        })
    }
}

// ── 校验工具 ─────────────────────────────────────────────────────────────────

pub fn validate_session_name(name: &str) -> anyhow::Result<()> {
    if name.is_empty() {
        anyhow::bail!("session name is required");
    }
    if name.len() > 64 {
        anyhow::bail!("session name must match ^[A-Za-z0-9._-]{{1,64}}$");
    }
    for c in name.chars() {
        if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
            continue;
        }
        anyhow::bail!("session name must match ^[A-Za-z0-9._-]{{1,64}}$");
    }
    Ok(())
}

pub fn validate_session_id(session_id: &str) -> anyhow::Result<()> {
    if session_id.is_empty() {
        anyhow::bail!("session_id is required");
    }
    if session_id.len() > 20 || !session_id.chars().all(|c| c.is_ascii_digit()) {
        anyhow::bail!("session_id must match ^[0-9]{{1,20}}$");
    }
    if session_id == "0" {
        anyhow::bail!("session_id must be greater than 0");
    }
    Ok(())
}

pub fn validate_resize(width: u32, height: u32) -> anyhow::Result<()> {
    if !(MIN_TERMINAL_WIDTH..=MAX_TERMINAL_WIDTH).contains(&width) {
        anyhow::bail!(
            "terminal width must be between {} and {}",
            MIN_TERMINAL_WIDTH,
            MAX_TERMINAL_WIDTH
        );
    }
    if !(MIN_TERMINAL_HEIGHT..=MAX_TERMINAL_HEIGHT).contains(&height) {
        anyhow::bail!(
            "terminal height must be between {} and {}",
            MIN_TERMINAL_HEIGHT,
            MAX_TERMINAL_HEIGHT
        );
    }
    Ok(())
}

pub fn validate_attach_size(width: u32, height: u32) -> anyhow::Result<()> {
    if width == 0 || height == 0 {
        anyhow::bail!("attach terminal size must set both width and height");
    }
    validate_resize(width, height)
}

pub fn validate_new_size(width: u32, height: u32) -> anyhow::Result<()> {
    if width == 0 && height == 0 {
        return Ok(());
    }
    if width == 0 || height == 0 {
        anyhow::bail!("terminal size must set both width and height or neither");
    }
    validate_resize(width, height)
}

// ── 错误消息 helper（与 Go 版本字符串一致）───────────────────────────────────

pub fn missing_session_error(session_id: &str) -> String {
    format!("session_id {:?} not found", session_id)
}

pub fn exited_session_error(session_id: &str) -> String {
    format!("session_id {:?} has exited", session_id)
}

pub fn attached_session_error(session_id: &str) -> String {
    format!("session_id {:?} is already attached", session_id)
}

// ── 单元测试 ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_request() {
        let msg = Message {
            kind: MessageKind::Request,
            id: "1".to_string(),
            command: Some(Command::New),
            session_id: "42".to_string(),
            name: "test".to_string(),
            shell: "cmd".to_string(),
            cwd: r"C:\work".to_string(),
            width: 80,
            height: 24,
            ..Default::default()
        };
        let line = msg.to_json_line().unwrap();
        let decoded = Message::from_json(std::str::from_utf8(&line).unwrap()).unwrap();
        assert_eq!(decoded.kind, MessageKind::Request);
        assert_eq!(decoded.id, "1");
        assert_eq!(decoded.command, Some(Command::New));
        assert_eq!(decoded.session_id, "42");
        assert_eq!(decoded.name, "test");
        assert_eq!(decoded.shell, "cmd");
        assert_eq!(decoded.cwd, r"C:\work");
        assert_eq!(decoded.width, 80);
        assert_eq!(decoded.height, 24);
    }

    #[test]
    fn omit_empty_shell() {
        let msg = Message {
            kind: MessageKind::Request,
            command: Some(Command::New),
            name: "test".to_string(),
            ..Default::default()
        };
        let line = msg.to_json_line().unwrap();
        let json_str = std::str::from_utf8(&line).unwrap();

        assert!(!json_str.contains("\"shell\""));
        assert!(!json_str.contains("\"cwd\""));
    }

    #[test]
    fn round_trip_focus_command() {
        let msg = Message {
            kind: MessageKind::Request,
            id: "focus-1".to_string(),
            command: Some(Command::Focus),
            session_id: "1".to_string(),
            ..Default::default()
        };
        let line = msg.to_json_line().unwrap();
        let json_str = std::str::from_utf8(&line).unwrap();
        assert!(json_str.contains(r#""command":"focus""#));
        let decoded = Message::from_json(json_str).unwrap();
        assert_eq!(decoded.command, Some(Command::Focus));
        assert_eq!(decoded.session_id, "1");
    }

    #[test]
    fn round_trip_rename_command() {
        let msg = Message {
            kind: MessageKind::Request,
            id: "rename-1".to_string(),
            command: Some(Command::Rename),
            session_id: "1".to_string(),
            name: "work".to_string(),
            ..Default::default()
        };
        let line = msg.to_json_line().unwrap();
        let decoded = Message::from_json(std::str::from_utf8(&line).unwrap()).unwrap();

        assert_eq!(decoded.command, Some(Command::Rename));
        assert_eq!(decoded.session_id, "1");
        assert_eq!(decoded.name, "work");
    }

    #[test]
    fn round_trip_payload() {
        let payload = b"hello\x1b[6n world".to_vec();
        let msg = Message {
            kind: MessageKind::Event,
            event: Some(EventType::Output),
            payload: payload.clone(),
            ..Default::default()
        };
        let line = msg.to_json_line().unwrap();
        // 检查 payload_b64 字段名兼容 Go
        let json_str = std::str::from_utf8(&line).unwrap();
        assert!(
            json_str.contains("payload_b64"),
            "must use payload_b64 field name"
        );
        let decoded = Message::from_json(json_str).unwrap();
        assert_eq!(decoded.payload, payload);
    }

    #[test]
    fn attach_mode_defaults_to_frame() {
        let decoded = Message::from_json(
            r#"{"kind":"request","id":"1","command":"attach","session_id":"42","width":80,"height":24}"#,
        )
        .unwrap();

        assert_eq!(decoded.attach_mode, AttachMode::Frame);

        let line = Message {
            kind: MessageKind::Request,
            command: Some(Command::Attach),
            attach_mode: AttachMode::Frame,
            ..Default::default()
        }
        .to_json_line()
        .unwrap();
        let json = std::str::from_utf8(&line).unwrap();
        assert!(!json.contains("attach_mode"));
    }

    #[test]
    fn attach_mode_bytes_round_trip() {
        let msg = Message {
            kind: MessageKind::Request,
            id: "1".to_string(),
            command: Some(Command::Attach),
            attach_mode: AttachMode::Bytes,
            ..Default::default()
        };
        let line = msg.to_json_line().unwrap();
        let json = std::str::from_utf8(&line).unwrap();

        assert!(json.contains(r#""attach_mode":"bytes""#));
        assert_eq!(
            Message::from_json(json).unwrap().attach_mode,
            AttachMode::Bytes
        );
    }

    #[test]
    fn output_event_uses_payload_b64() {
        let msg = Message {
            kind: MessageKind::Event,
            event: Some(EventType::Output),
            payload: b"bytes".to_vec(),
            ..Default::default()
        };
        let line = msg.to_json_line().unwrap();
        let json = std::str::from_utf8(&line).unwrap();

        assert!(json.contains(r#""event":"output""#));
        assert!(json.contains("payload_b64"));
        assert!(!json.contains(r#""payload":"#));
    }

    #[test]
    fn output_payload_over_limit_is_rejected() {
        let msg = Message {
            kind: MessageKind::Event,
            event: Some(EventType::Output),
            payload: vec![b'x'; MAX_PAYLOAD_SIZE + 1],
            ..Default::default()
        };

        assert!(msg.to_json_line().is_err());
    }

    #[test]
    fn round_trip_frame_event_type() {
        let frame = ScreenFrame {
            rows: 1,
            cols: 5,
            cursor_shape: 5,
            rows_v2: vec![vec![ScreenRun {
                text: "hello".to_string(),
                fg: FrameColor::Default,
                bg: FrameColor::Idx(4),
                flags: FRAME_FLAG_BOLD,
                width: 5,
            }]],
            ..Default::default()
        };
        let msg = Message {
            kind: MessageKind::Event,
            event: Some(EventType::Frame),
            frame: Some(frame.clone()),
            ..Default::default()
        };

        let line = msg.to_json_line().unwrap();
        let decoded = Message::from_json(std::str::from_utf8(&line).unwrap()).unwrap();

        assert_eq!(decoded.event, Some(EventType::Frame));
        assert_eq!(decoded.frame, Some(frame));
        let json = std::str::from_utf8(&line).unwrap();
        assert!(json.contains(r#""frame":"#));
        assert!(json.contains(r#""cursor_shape":5"#));
        assert!(!json.contains("payload_b64"));
    }

    #[test]
    fn frame_event_uses_frame_size_limit() {
        let long_text = "x".repeat(MAX_PAYLOAD_SIZE + 1);
        let frame = ScreenFrame {
            rows: 1,
            cols: 1,
            rows_v2: vec![vec![ScreenRun {
                text: long_text.clone(),
                fg: FrameColor::Default,
                bg: FrameColor::Default,
                flags: 0,
                width: 1,
            }]],
            ..Default::default()
        };
        let msg = Message {
            kind: MessageKind::Event,
            event: Some(EventType::Frame),
            frame: Some(frame),
            ..Default::default()
        };

        let line = msg.to_json_line().unwrap();
        let decoded = Message::from_json(std::str::from_utf8(&line).unwrap()).unwrap();

        assert_eq!(
            decoded.frame.unwrap().rows_v2[0][0].text.len(),
            long_text.len()
        );
    }

    #[test]
    fn frame_event_over_frame_size_limit_is_rejected() {
        let long_text = "x".repeat(MAX_FRAME_MESSAGE_SIZE + 1);
        let frame = ScreenFrame {
            rows: 1,
            cols: 1,
            rows_v2: vec![vec![ScreenRun {
                text: long_text,
                fg: FrameColor::Default,
                bg: FrameColor::Default,
                flags: 0,
                width: 1,
            }]],
            ..Default::default()
        };
        let msg = Message {
            kind: MessageKind::Event,
            event: Some(EventType::Frame),
            frame: Some(frame),
            ..Default::default()
        };

        assert!(msg.to_json_line().is_err());
    }

    #[test]
    fn frame_null_does_not_bypass_wire_size_limit() {
        let oversized_error = "x".repeat(MAX_WIRE_MESSAGE_SIZE + 1);
        let json = format!(
            r#"{{"kind":"response","id":"1","error":"{}","frame":null}}"#,
            oversized_error
        );

        assert!(Message::from_json(&json).is_err());
    }

    #[test]
    fn omit_false_ok() {
        let msg = Message {
            kind: MessageKind::Response,
            id: "2".to_string(),
            ok: false,
            ..Default::default()
        };
        let line = msg.to_json_line().unwrap();
        let json_str = std::str::from_utf8(&line).unwrap();
        assert!(!json_str.contains("\"ok\""), "ok=false must be omitted");
    }

    #[test]
    fn include_true_ok() {
        let msg = Message {
            kind: MessageKind::Response,
            id: "3".to_string(),
            ok: true,
            ..Default::default()
        };
        let line = msg.to_json_line().unwrap();
        let json_str = std::str::from_utf8(&line).unwrap();
        assert!(json_str.contains("\"ok\":true"), "ok=true must be included");
    }

    #[test]
    fn validate_session_name_ok() {
        validate_session_name("main").unwrap();
        validate_session_name("test-session_01").unwrap();
        validate_session_name("a").unwrap();
    }

    #[test]
    fn validate_session_name_err() {
        assert!(validate_session_name("").is_err());
        assert!(validate_session_name("bad name").is_err());
        assert!(validate_session_name(&"x".repeat(65)).is_err());
    }

    #[test]
    fn validate_session_id_ok() {
        validate_session_id("1").unwrap();
        validate_session_id("42").unwrap();
    }

    #[test]
    fn validate_session_id_err() {
        assert!(validate_session_id("").is_err());
        assert!(validate_session_id("0").is_err());
        assert!(validate_session_id("bad").is_err());
        assert!(validate_session_id("1.2").is_err());
        assert!(validate_session_id(&"1".repeat(21)).is_err());
    }

    #[test]
    fn validate_attach_size_rejects_missing_width() {
        let msg = Message::from_json(
            r#"{"kind":"request","id":"1","command":"attach","session_id":"1","height":24}"#,
        )
        .unwrap();
        assert!(validate_attach_size(msg.width, msg.height).is_err());
    }

    #[test]
    fn validate_attach_size_rejects_missing_height() {
        let msg = Message::from_json(
            r#"{"kind":"request","id":"1","command":"attach","session_id":"1","width":80}"#,
        )
        .unwrap();
        assert!(validate_attach_size(msg.width, msg.height).is_err());
    }

    #[test]
    fn validate_attach_size_rejects_zero_width() {
        assert!(validate_attach_size(0, 24).is_err());
    }

    #[test]
    fn validate_attach_size_rejects_zero_height() {
        assert!(validate_attach_size(80, 0).is_err());
    }

    #[test]
    fn validate_attach_size_accepts_valid_size() {
        validate_attach_size(80, 24).unwrap();
    }

    #[test]
    fn validate_attach_size_accepts_quicktui_max_size() {
        validate_attach_size(1000, 500).unwrap();
    }

    #[test]
    fn validate_attach_size_rejects_above_quicktui_max_width() {
        assert!(validate_attach_size(1001, 500).is_err());
    }

    #[test]
    fn validate_attach_size_rejects_above_quicktui_max_height() {
        assert!(validate_attach_size(1000, 501).is_err());
    }
}
