use base64::{Engine, engine::general_purpose::STANDARD as B64};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// 与 Go wire.go 保持字节兼容的常量
pub const MAX_CONTROL_MESSAGE_SIZE: usize = 64 * 1024;
pub const MAX_PAYLOAD_SIZE: usize = 64 * 1024;
pub const MAX_WIRE_MESSAGE_SIZE: usize =
    MAX_CONTROL_MESSAGE_SIZE + 4 * MAX_PAYLOAD_SIZE.div_ceil(3);

pub const MIN_TERMINAL_WIDTH: u32 = 1;
pub const MAX_TERMINAL_WIDTH: u32 = 500;
pub const MIN_TERMINAL_HEIGHT: u32 = 1;
pub const MAX_TERMINAL_HEIGHT: u32 = 200;

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
    Input,
    Resize,
    Kill,
    Stop,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EventType {
    Output,
    Exit,
}

// ── SessionInfo ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SessionInfo {
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
}

// ── Message (公开 API) ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct Message {
    pub kind: MessageKind,
    pub id: String,
    pub command: Option<Command>,
    pub event: Option<EventType>,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub ok: bool,
    pub error: String,
    pub sessions: Vec<SessionInfo>,
    pub exit_code: i64,
    pub payload: Vec<u8>,
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
    name: String,
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
}

fn is_false(v: &bool) -> bool {
    !v
}
fn is_zero_u32(v: &u32) -> bool {
    *v == 0
}
fn is_zero_i64(v: &i64) -> bool {
    *v == 0
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
            name: self.name.clone(),
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
        };
        let mut bytes = serde_json::to_vec(&wire)?;
        bytes.push(b'\n');
        Ok(bytes)
    }

    /// 从 JSON 字符串（含可选尾部空白）解析
    pub fn from_json(s: &str) -> anyhow::Result<Self> {
        let s = s.trim();
        if s.len() > MAX_WIRE_MESSAGE_SIZE {
            anyhow::bail!("message too large: {} > {}", s.len(), MAX_WIRE_MESSAGE_SIZE);
        }
        let wire: WireMessage = serde_json::from_str(s)?;
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
            name: wire.name,
            width: wire.width,
            height: wire.height,
            ok: wire.ok,
            error: wire.error,
            sessions: wire.sessions,
            exit_code: wire.exit_code,
            payload,
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

pub fn duplicate_session_error(name: &str) -> String {
    format!("session {:?} already exists", name)
}

pub fn missing_session_error(name: &str) -> String {
    format!("session {:?} not found", name)
}

pub fn exited_session_error(name: &str) -> String {
    format!("session {:?} has exited", name)
}

pub fn attached_session_error(name: &str) -> String {
    format!("session {:?} is already attached", name)
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
            name: "test".to_string(),
            width: 80,
            height: 24,
            ..Default::default()
        };
        let line = msg.to_json_line().unwrap();
        let decoded = Message::from_json(std::str::from_utf8(&line).unwrap()).unwrap();
        assert_eq!(decoded.kind, MessageKind::Request);
        assert_eq!(decoded.id, "1");
        assert_eq!(decoded.command, Some(Command::New));
        assert_eq!(decoded.name, "test");
        assert_eq!(decoded.width, 80);
        assert_eq!(decoded.height, 24);
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
}
