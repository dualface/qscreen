use std::io::Write;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Context;
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use qscreen_protocol::{
    Command, EventType, Message, MessageKind, SessionInfo, attached_session_error,
    exited_session_error, validate_session_name,
};
use qscreen_shared::{daemon_log_path, pipe_name};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

mod term;

const DEFAULT_PREFIX: PrefixKey = PrefixKey {
    ctrl_char: 'A',
    byte: 0x01,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PrefixKey {
    ctrl_char: char,
    byte: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ClientConfig {
    prefix: PrefixKey,
}

impl PrefixKey {
    fn parse(value: &str) -> anyhow::Result<Self> {
        let value = value.trim();
        if value.is_empty() {
            anyhow::bail!(
                "invalid prefix: value is empty; expected C-a through C-z or Ctrl+A through Ctrl+Z"
            );
        }

        let ctrl_char = if let Some(rest) = value
            .strip_prefix("C-")
            .or_else(|| value.strip_prefix("c-"))
        {
            parse_prefix_letter(value, rest)?
        } else {
            let lower = value.to_ascii_lowercase();
            if !lower.starts_with("ctrl+") {
                anyhow::bail!(
                    "invalid prefix `{}`: expected C-a through C-z or Ctrl+A through Ctrl+Z",
                    value
                );
            }
            parse_prefix_letter(value, &value[5..])?
        };

        Ok(Self {
            ctrl_char,
            byte: ctrl_char as u8 - b'A' + 1,
        })
    }
}

fn parse_prefix_letter(original: &str, rest: &str) -> anyhow::Result<char> {
    let mut chars = rest.chars();
    let Some(letter) = chars.next() else {
        anyhow::bail!(
            "invalid prefix `{}`: missing control letter; expected A through Z",
            original
        );
    };
    if chars.next().is_some() {
        anyhow::bail!(
            "invalid prefix `{}`: expected exactly one control letter",
            original
        );
    }
    if !letter.is_ascii_alphabetic() {
        anyhow::bail!(
            "invalid prefix `{}`: control key must be a letter A through Z",
            original
        );
    }
    Ok(letter.to_ascii_uppercase())
}

fn parse_client_config(args: Vec<String>) -> anyhow::Result<(ClientConfig, Vec<String>)> {
    parse_client_config_with_env(args, std::env::var("QSCREEN_PREFIX").ok())
}

fn parse_client_config_with_env(
    args: Vec<String>,
    env_prefix: Option<String>,
) -> anyhow::Result<(ClientConfig, Vec<String>)> {
    let (prefix_arg, remaining_args) = take_prefix_arg(args)?;
    let prefix = match prefix_arg.or(env_prefix) {
        Some(value) => PrefixKey::parse(&value)?,
        None => DEFAULT_PREFIX,
    };
    Ok((ClientConfig { prefix }, remaining_args))
}

fn take_prefix_arg(args: Vec<String>) -> anyhow::Result<(Option<String>, Vec<String>)> {
    let mut prefix = None;
    let mut remaining = Vec::with_capacity(args.len());
    let mut iter = args.into_iter();

    while let Some(arg) = iter.next() {
        if arg == "--prefix" {
            let Some(value) = iter.next() else {
                anyhow::bail!("invalid prefix: --prefix requires a value");
            };
            prefix = Some(value);
        } else if let Some(value) = arg.strip_prefix("--prefix=") {
            prefix = Some(value.to_string());
        } else {
            remaining.push(arg);
        }
    }

    Ok((prefix, remaining))
}

// ── 入口 ─────────────────────────────────────────────────────────────────────

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // --daemon 模式：启动 daemon 服务器
    if args.first().map(|s| s.as_str()) == Some("--daemon") {
        run_daemon_mode();
        return;
    }

    // CLI client 模式
    if let Err(e) = run_client(args) {
        eprintln!("{}", e);
        std::process::exit(1);
    }
}

// ── Daemon 模式 ───────────────────────────────────────────────────────────────

fn run_daemon_mode() {
    let log_path = daemon_log_path();
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .unwrap_or_else(|_| panic!("cannot open daemon log {}", log_path.display()));

    tracing_subscriber::fmt()
        .with_writer(std::sync::Mutex::new(log_file))
        .with_ansi(false)
        .with_target(false)
        .init();

    tracing::info!("daemon process started pid={}", std::process::id());

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    if let Err(e) = rt.block_on(qscreen_daemon::run()) {
        tracing::error!("daemon error: {}", e);
        std::process::exit(1);
    }
}

// ── Client 模式 ───────────────────────────────────────────────────────────────

fn run_client(args: Vec<String>) -> anyhow::Result<()> {
    let (config, args) = parse_client_config(args)?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    rt.block_on(async {
        match args.as_slice() {
            [] => cmd_default(config).await,
            [cmd] if cmd == "-h" || cmd == "--help" => {
                print_help();
                Ok(())
            }
            [cmd] if cmd == "ls" || cmd == "list" => cmd_list().await,
            [cmd] if cmd == "shutdown" => cmd_shutdown().await,
            [cmd, name] if cmd == "new" => cmd_new(name, config).await,
            [cmd, name] if cmd == "attach" || cmd == "-r" => cmd_attach(name, config).await,
            [cmd, name] if cmd == "kill" => cmd_kill(name).await,
            [cmd] if cmd == "new" => {
                let name = default_new_name();
                cmd_new_and_attach(&name, config).await
            }
            _ => {
                if is_chinese() {
                    anyhow::bail!("未知命令。运行 `qscn --help` 查看帮助")
                } else {
                    anyhow::bail!("unknown command. Run `qscn --help` for usage")
                }
            }
        }
    })
}

fn default_new_name() -> String {
    use chrono::Utc;
    Utc::now().format("%Y%m%d-%H%M%S").to_string()
}

// ── 语言检测 ──────────────────────────────────────────────────────────────────

static IS_CHINESE: OnceLock<bool> = OnceLock::new();

fn is_chinese() -> bool {
    *IS_CHINESE.get_or_init(detect_chinese)
}

fn detect_chinese() -> bool {
    for var in ["LANG", "LANGUAGE", "LC_ALL", "LC_MESSAGES"] {
        if let Ok(val) = std::env::var(var)
            && !val.is_empty()
        {
            return val.to_lowercase().contains("zh");
        }
    }
    #[cfg(windows)]
    {
        return windows_locale_is_chinese();
    }
    #[cfg(not(windows))]
    false
}

#[cfg(windows)]
fn windows_locale_is_chinese() -> bool {
    unsafe extern "system" {
        fn GetUserDefaultLocaleName(lp_locale_name: *mut u16, cch_locale_name: i32) -> i32;
    }
    let mut buf = [0u16; 85];
    let len = unsafe { GetUserDefaultLocaleName(buf.as_mut_ptr(), buf.len() as i32) };
    if len > 1 {
        let name = String::from_utf16_lossy(&buf[..len as usize - 1]);
        return name.to_lowercase().starts_with("zh");
    }
    false
}

// ── 帮助文本 ──────────────────────────────────────────────────────────────────

fn print_help() {
    if is_chinese() {
        println!(
            r#"qscreen — 轻量终端会话管理器

用法:
  qscn [--prefix C-a]          智能启动：无会话时新建并进入 main，单会话时直接 attach，
                            多会话时列出所有会话
  qscn [--prefix C-a] new [<name>]
                               新建会话并进入（省略 name 时自动用时间戳命名）
  qscn [--prefix C-a] attach <name>
                               进入已有会话
  qscn [--prefix C-a] -r <name>
                               同 attach，兼容 tmux 风格
  qscn ls                      列出所有会话（同 list）
  qscn list                    列出所有会话
  qscn kill <name>             强制终止指定会话
  qscn shutdown                停止后台 daemon（所有会话将被关闭）
  qscn -h, --help              显示此帮助

前缀:
  --prefix C-b                 使用 Ctrl+B 作为当前命令的会话前缀
  QSCREEN_PREFIX=C-b           为所有命令设置备用前缀
  支持 C-a..C-z 或 Ctrl+A..Ctrl+Z；CLI 参数优先于环境变量

会话内热键:
  <prefix> d                  从当前会话 detach（会话继续在后台运行）
  <prefix> <prefix>           向 PTY 发送字面前缀字符
  <prefix> s                  打开会话列表，选择 detached 会话后切换 attach

ls 输出格式:
  <name>  <状态>  <创建时间>  <终端尺寸>
  状态: attached | detached | exited(<退出码>)

示例:
  qscn                         # 自动进入唯一会话，或新建 main
  qscn new work                # 新建名为 work 的会话
  qscn --prefix C-b attach work # 使用 Ctrl+B 作为前缀进入 work
  qscn attach work             # 重新进入 work 会话
  qscn ls                      # 查看所有会话状态
  qscn kill work               # 终止 work 会话
"#
        );
    } else {
        println!(
            r#"qscreen — lightweight terminal session manager

Usage:
  qscn [--prefix C-a]          smart launch: create and enter 'main' if no sessions,
                            attach if one session, list all if multiple
  qscn [--prefix C-a] new [<name>]
                               create a new session and attach (auto-name if omitted)
  qscn [--prefix C-a] attach <name>
                               attach to an existing session
  qscn [--prefix C-a] -r <name>
                               same as attach (tmux-style shorthand)
  qscn ls                      list all sessions (alias: list)
  qscn list                    list all sessions
  qscn kill <name>             forcibly terminate a session
  qscn shutdown                stop the background daemon (closes all sessions)
  qscn -h, --help              show this help

Prefix:
  --prefix C-b                 use Ctrl+B as the session prefix for this command
  QSCREEN_PREFIX=C-b           set a fallback prefix for every command
  Values: C-a..C-z or Ctrl+A..Ctrl+Z; CLI takes precedence over env

Key bindings (inside a session):
  <prefix> d                  detach from session (session keeps running)
  <prefix> <prefix>           send a literal prefix key to the PTY
  <prefix> s                  open the session list and switch to a detached session

ls output format:
  <name>  <state>  <created-at>  <terminal-size>
  states: attached | detached | exited(<code>)

Examples:
  qscn                         # auto-attach or create main
  qscn new work                # create session named 'work'
  qscn --prefix C-b attach work # attach using Ctrl+B as the prefix
  qscn attach work             # reattach to 'work'
  qscn ls                      # show all session states
  qscn kill work               # terminate 'work'
"#
        );
    }
}

// ── 子命令实现 ────────────────────────────────────────────────────────────────

async fn cmd_default(config: ClientConfig) -> anyhow::Result<()> {
    let sessions = list_sessions().await?;
    match sessions.len() {
        0 => cmd_new_and_attach("main", config).await,
        1 => cmd_attach(&sessions[0].name.clone(), config).await,
        _ => {
            print_sessions(&sessions);
            Ok(())
        }
    }
}

async fn cmd_list() -> anyhow::Result<()> {
    let sessions = list_sessions().await?;
    print_sessions(&sessions);
    Ok(())
}

async fn cmd_new(name: &str, config: ClientConfig) -> anyhow::Result<()> {
    cmd_new_and_attach(name, config).await
}

async fn cmd_new_and_attach(name: &str, config: ClientConfig) -> anyhow::Result<()> {
    validate_session_name(name)?;
    let mut conn = ensure_and_connect().await?;
    send_recv_ok(
        &mut conn,
        Message {
            kind: MessageKind::Request,
            id: "1".to_string(),
            command: Some(Command::New),
            name: name.to_string(),
            ..Default::default()
        },
    )
    .await?;
    drop(conn);
    attach_session_loop(name, config).await
}

async fn cmd_attach(name: &str, config: ClientConfig) -> anyhow::Result<()> {
    validate_session_name(name)?;
    attach_session_loop(name, config).await
}

async fn cmd_kill(name: &str) -> anyhow::Result<()> {
    validate_session_name(name)?;
    let mut conn = ensure_and_connect().await?;
    send_recv_ok(
        &mut conn,
        Message {
            kind: MessageKind::Request,
            id: "1".to_string(),
            command: Some(Command::Kill),
            name: name.to_string(),
            ..Default::default()
        },
    )
    .await
}

async fn cmd_shutdown() -> anyhow::Result<()> {
    match connect().await {
        Err(_) => Ok(()),
        Ok(mut conn) => {
            send_recv_ok(
                &mut conn,
                Message {
                    kind: MessageKind::Request,
                    id: "1".to_string(),
                    command: Some(Command::Stop),
                    ..Default::default()
                },
            )
            .await?;
            Ok(())
        }
    }
}

async fn list_sessions() -> anyhow::Result<Vec<SessionInfo>> {
    let mut conn = ensure_and_connect().await?;
    send_msg(
        &mut conn,
        Message {
            kind: MessageKind::Request,
            id: "1".to_string(),
            command: Some(Command::List),
            ..Default::default()
        },
    )
    .await?;
    let resp = recv_msg(&mut conn).await?;
    check_response(&resp, "1")?;
    Ok(resp.sessions)
}

fn print_sessions(sessions: &[SessionInfo]) {
    for s in sessions {
        let created = if s.created_at.timestamp() == 0 {
            "-".to_string()
        } else {
            s.created_at.format("%Y-%m-%dT%H:%M:%SZ").to_string()
        };
        println!(
            "{}\t{}\t{}\t{}",
            s.name,
            session_state_label(s),
            created,
            session_size_label(s)
        );
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionListRow {
    name: String,
    state: String,
    size: String,
    is_current: bool,
    exited: bool,
    attached: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SessionListSelection {
    Close,
    Error(String),
    Switch(String),
}

fn build_session_list_rows(sessions: &[SessionInfo], current_name: &str) -> Vec<SessionListRow> {
    let mut rows: Vec<SessionListRow> = sessions
        .iter()
        .map(|session| SessionListRow {
            name: session.name.clone(),
            state: session_state_label(session),
            size: session_size_label(session),
            is_current: session.name == current_name,
            exited: session.exited,
            attached: session.attached,
        })
        .collect();
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    rows
}

fn session_state_label(session: &SessionInfo) -> String {
    if session.exited {
        format!("exited({})", session.exit_code)
    } else if session.attached {
        "attached".to_string()
    } else {
        "detached".to_string()
    }
}

fn session_size_label(session: &SessionInfo) -> String {
    if session.size.is_empty() {
        format!("{}x{}", session.width, session.height)
    } else {
        session.size.clone()
    }
}

fn selection_for_session_row(row: &SessionListRow) -> SessionListSelection {
    if row.is_current {
        SessionListSelection::Close
    } else if row.exited {
        SessionListSelection::Error(exited_session_error(&row.name))
    } else if row.attached {
        SessionListSelection::Error(attached_session_error(&row.name))
    } else {
        SessionListSelection::Switch(row.name.clone())
    }
}

fn move_session_list_selection(selected: usize, len: usize, delta: isize) -> usize {
    if len == 0 {
        return 0;
    }

    if delta < 0 {
        selected.saturating_sub(delta.unsigned_abs())
    } else {
        (selected + delta as usize).min(len - 1)
    }
}

// ── Attach 实现 ───────────────────────────────────────────────────────────────

async fn attach_session_loop(initial_name: &str, config: ClientConfig) -> anyhow::Result<()> {
    let mut name = initial_name.to_string();

    loop {
        validate_session_name(&name)?;
        let outcome = attach_session_once(&name, config).await?;
        match next_attach_target_after_outcome(outcome) {
            Some(next_name) => name = next_name,
            None => return Ok(()),
        }
    }
}

fn next_attach_target_after_outcome(outcome: AttachOutcome) -> Option<String> {
    match outcome {
        AttachOutcome::SwitchTo(next_name) => Some(next_name),
        AttachOutcome::Detached | AttachOutcome::Ended => None,
    }
}

async fn attach_session_once(name: &str, config: ClientConfig) -> anyhow::Result<AttachOutcome> {
    let mut conn = ensure_and_connect().await?;

    let attach_id = "1";
    send_msg(
        &mut conn,
        Message {
            kind: MessageKind::Request,
            id: attach_id.to_string(),
            command: Some(Command::Attach),
            name: name.to_string(),
            ..Default::default()
        },
    )
    .await?;

    let resp = recv_msg(&mut conn).await?;
    check_response(&resp, attach_id)?;

    let term_size = get_terminal_size().unwrap_or((80, 24));

    {
        let (w, h) = term_size;
        let _ = send_msg(
            &mut conn,
            Message {
                kind: MessageKind::Request,
                id: "2".to_string(),
                command: Some(Command::Resize),
                name: name.to_string(),
                width: w as u32,
                height: h as u32,
                ..Default::default()
            },
        )
        .await;
    }

    let _terminal = TerminalCleanupGuard::enter()?;

    let name_owned = name.to_string();
    run_attach_loop(conn, name_owned, term_size, config.prefix).await
}

struct TerminalCleanupGuard;

impl TerminalCleanupGuard {
    fn enter() -> anyhow::Result<Self> {
        crossterm::terminal::enable_raw_mode()?;

        #[cfg(windows)]
        {
            let _ = std::io::stdout().write_all(b"\x1b[?9001l");
            let _ = std::io::stdout().flush();
        }

        Ok(Self)
    }
}

impl Drop for TerminalCleanupGuard {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = std::io::stdout().write_all(
            b"\x1b[?2026l\x1b[?2004l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1004l\x1b[?25h\x1b[0m\x1b[r",
        );
        #[cfg(windows)]
        let _ = std::io::stdout().write_all(b"\x1b[?9001l\x1b[!p");
        let _ = std::io::stdout().flush();
    }
}

// ── 键盘事件 → PTY 字节序列 ───────────────────────────────────────────────────

fn key_event_to_bytes(event: crossterm::event::KeyEvent) -> Vec<u8> {
    let ctrl = event.modifiers.contains(KeyModifiers::CONTROL);
    let alt = event.modifiers.contains(KeyModifiers::ALT);

    match event.code {
        // Backspace → BS (0x08)，Windows Terminal raw mode 发 DEL (0x7f) 会让 PSReadLine 误删整行
        KeyCode::Backspace => vec![0x08],
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Delete => vec![0x7f],

        KeyCode::Up => vec![0x1b, b'[', b'A'],
        KeyCode::Down => vec![0x1b, b'[', b'B'],
        KeyCode::Right => vec![0x1b, b'[', b'C'],
        KeyCode::Left => vec![0x1b, b'[', b'D'],
        KeyCode::Home => vec![0x1b, b'[', b'H'],
        KeyCode::End => vec![0x1b, b'[', b'F'],
        KeyCode::PageUp => vec![0x1b, b'[', b'5', b'~'],
        KeyCode::PageDown => vec![0x1b, b'[', b'6', b'~'],
        KeyCode::Insert => vec![0x1b, b'[', b'2', b'~'],

        KeyCode::F(1) => vec![0x1b, b'O', b'P'],
        KeyCode::F(2) => vec![0x1b, b'O', b'Q'],
        KeyCode::F(3) => vec![0x1b, b'O', b'R'],
        KeyCode::F(4) => vec![0x1b, b'O', b'S'],
        KeyCode::F(n @ 5..=12) => {
            let code: &[u8] = match n {
                5 => b"15",
                6 => b"17",
                7 => b"18",
                8 => b"19",
                9 => b"20",
                10 => b"21",
                11 => b"23",
                12 => b"24",
                _ => return vec![],
            };
            let mut v = vec![0x1b, b'['];
            v.extend_from_slice(code);
            v.push(b'~');
            v
        }

        KeyCode::Char(c) if ctrl => match c {
            'a'..='z' => vec![c as u8 - b'a' + 1],
            'A'..='Z' => vec![c as u8 - b'A' + 1],
            ' ' => vec![0],
            _ => c.to_string().into_bytes(),
        },

        KeyCode::Char(c) if alt => {
            let mut v = vec![0x1b];
            v.extend_from_slice(c.to_string().as_bytes());
            v
        }

        KeyCode::Char(c) => c.to_string().into_bytes(),

        _ => vec![],
    }
}

// ── Attach 主循环 ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
enum AttachAction {
    Input(Vec<u8>),
    Resize(u16, u16),
    Detach,
    OpenSessionList,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SessionListAction {
    MoveUp,
    MoveDown,
    Select,
    Cancel,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AttachOutcome {
    Detached,
    SwitchTo(String),
    Ended,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct PrefixState {
    pending: bool,
}

impl PrefixState {
    fn handle_key(
        &mut self,
        key_event: crossterm::event::KeyEvent,
        prefix: PrefixKey,
    ) -> Vec<AttachAction> {
        if is_prefix_key_event(key_event, prefix) {
            if self.pending {
                self.pending = false;
                return vec![AttachAction::Input(vec![prefix.byte])];
            }
            self.pending = true;
            return Vec::new();
        }

        if self.pending {
            self.pending = false;
            if key_char_eq_ignore_ascii_case(key_event.code, 'd') {
                return vec![AttachAction::Detach];
            }
            if key_char_eq_ignore_ascii_case(key_event.code, 's') && session_list_action_enabled() {
                return vec![AttachAction::OpenSessionList];
            }

            let mut actions = vec![AttachAction::Input(vec![prefix.byte])];
            let bytes = key_event_to_bytes(key_event);
            if !bytes.is_empty() {
                actions.push(AttachAction::Input(bytes));
            }
            return actions;
        }

        let bytes = key_event_to_bytes(key_event);
        if bytes.is_empty() {
            Vec::new()
        } else {
            vec![AttachAction::Input(bytes)]
        }
    }
}

async fn run_attach_loop(
    conn: TcpConn,
    name: String,
    term_size: (u16, u16),
    prefix: PrefixKey,
) -> anyhow::Result<AttachOutcome> {
    let (read_half, write_half) = tokio::io::split(conn.stream);
    let writer = Arc::new(tokio::sync::Mutex::new(write_half));
    let mut reader = BufReader::new(read_half);

    let (cols, rows) = term_size;
    let mut screen = term::TermScreen::new(rows, cols);

    let writer_c = writer.clone();
    let name_c = name.clone();
    let mut msg_id: u64 = 10;

    let (action_tx, mut action_rx) = tokio::sync::mpsc::unbounded_channel::<AttachAction>();
    let stop_input = Arc::new(AtomicBool::new(false));
    let mut input_handle = spawn_attach_input_reader(action_tx.clone(), stop_input.clone(), prefix);

    let mut stdout = std::io::stdout();
    let mut line = String::new();

    let outcome = loop {
        line.clear();
        tokio::select! {
            result = reader.read_line(&mut line) => {
                match result {
                    Ok(0) | Err(_) => break AttachOutcome::Ended,
                    Ok(_) => {}
                }
                let msg = match Message::from_json(&line) {
                    Ok(m) => m,
                    Err(_) => break AttachOutcome::Ended,
                };
                match msg.kind {
                    MessageKind::Event => match msg.event {
                        Some(EventType::Output) => {
                            screen.process(&msg.payload);
                            let _ = screen.render(&mut stdout);
                        }
                        Some(EventType::Exit) => break AttachOutcome::Ended,
                        _ => {}
                    },
                    MessageKind::Response if !msg.error.is_empty() => {
                        break AttachOutcome::Ended;
                    }
                    _ => {}
                }
            }

            action = action_rx.recv() => {
                match action {
                    None => break AttachOutcome::Ended,
                    Some(AttachAction::Input(data)) => {
                        msg_id += 1;
                        let input_msg = Message {
                            kind: MessageKind::Request,
                            id: msg_id.to_string(),
                            command: Some(Command::Input),
                            name: name_c.clone(),
                            payload: data,
                            ..Default::default()
                        };
                        let bytes = input_msg.to_json_line()?;
                        if writer_c.lock().await.write_all(&bytes).await.is_err() {
                            break AttachOutcome::Ended;
                        }
                    }
                    Some(AttachAction::Resize(w, h)) => {
                        screen.resize(h, w);
                        msg_id += 1;
                        let resize_msg = Message {
                            kind: MessageKind::Request,
                            id: msg_id.to_string(),
                            command: Some(Command::Resize),
                            name: name_c.clone(),
                            width: w as u32,
                            height: h as u32,
                            ..Default::default()
                        };
                        let bytes = resize_msg.to_json_line()?;
                        if writer_c.lock().await.write_all(&bytes).await.is_err() {
                            break AttachOutcome::Ended;
                        }
                    }
                    Some(AttachAction::Detach) => {
                        msg_id += 1;
                        let detach_msg = Message {
                            kind: MessageKind::Request,
                            id: msg_id.to_string(),
                            command: Some(Command::Detach),
                            name: name_c.clone(),
                            ..Default::default()
                        };
                        let bytes = detach_msg.to_json_line()?;
                        let _ = writer_c.lock().await.write_all(&bytes).await;
                        break AttachOutcome::Detached;
                    }
                    Some(AttachAction::OpenSessionList) => {
                        stop_input.store(true, Ordering::Relaxed);
                        let _ = input_handle.await;

                        match run_session_list_mode(&name_c, screen.size(), &mut stdout, &mut screen).await? {
                            SessionListSelection::Switch(next_name) => {
                                break AttachOutcome::SwitchTo(next_name);
                            }
                            SessionListSelection::Close | SessionListSelection::Error(_) => {
                                screen.force_redraw();
                                let _ = screen.render(&mut stdout);
                                stop_input.store(false, Ordering::Relaxed);
                                input_handle = spawn_attach_input_reader(
                                    action_tx.clone(),
                                    stop_input.clone(),
                                    prefix,
                                );
                            }
                        }
                    }
                }
            }
        }
    };

    stop_input.store(true, Ordering::Relaxed);
    Ok(outcome)
}

fn spawn_attach_input_reader(
    action_tx: tokio::sync::mpsc::UnboundedSender<AttachAction>,
    stop_input: Arc<AtomicBool>,
    prefix: PrefixKey,
) -> tokio::task::JoinHandle<()> {
    // Keyboard/resize reading uses a bounded poll so attach cleanup can finish without process exit.
    tokio::task::spawn_blocking(move || {
        let mut prefix_state = PrefixState::default();
        while !stop_input.load(Ordering::Relaxed) {
            let event = match crossterm::event::poll(Duration::from_millis(50)) {
                Ok(true) => crossterm::event::read(),
                Ok(false) => continue,
                Err(_) => break,
            };
            let Ok(event) = event else {
                break;
            };
            match event {
                // 只处理按键按下事件，避免 key-up 重复输入
                Event::Key(key_event)
                    if matches!(key_event.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                {
                    for action in prefix_state.handle_key(key_event, prefix) {
                        let should_stop =
                            matches!(action, AttachAction::Detach | AttachAction::OpenSessionList);
                        let _ = action_tx.send(action);
                        if should_stop {
                            return;
                        }
                    }
                }

                Event::Resize(w, h) => {
                    let _ = action_tx.send(AttachAction::Resize(w, h));
                }

                _ => {}
            }
        }
    })
}

async fn run_session_list_mode<W: Write>(
    current_name: &str,
    term_size: (u16, u16),
    stdout: &mut W,
    screen: &mut term::TermScreen,
) -> anyhow::Result<SessionListSelection> {
    let mut rows = build_session_list_rows(&list_sessions().await?, current_name);
    let mut selected = rows
        .iter()
        .position(|row| row.is_current)
        .unwrap_or_default();
    let mut status = String::new();

    render_session_list(stdout, &rows, selected, &status, term_size)?;

    loop {
        let action = read_session_list_action().await?;
        match action {
            SessionListAction::MoveUp => {
                selected = move_session_list_selection(selected, rows.len(), -1);
                render_session_list(stdout, &rows, selected, &status, term_size)?;
            }
            SessionListAction::MoveDown => {
                selected = move_session_list_selection(selected, rows.len(), 1);
                render_session_list(stdout, &rows, selected, &status, term_size)?;
            }
            SessionListAction::Cancel => return Ok(SessionListSelection::Close),
            SessionListAction::Select => {
                if rows.is_empty() {
                    status = "no sessions".to_string();
                    render_session_list(stdout, &rows, selected, &status, term_size)?;
                    continue;
                }

                rows = build_session_list_rows(&list_sessions().await?, current_name);
                if rows.is_empty() {
                    selected = 0;
                    status = "no sessions".to_string();
                    render_session_list(stdout, &rows, selected, &status, term_size)?;
                    continue;
                }
                selected = selected.min(rows.len().saturating_sub(1));
                let selection = selection_for_session_row(&rows[selected]);
                match selection {
                    SessionListSelection::Close => return Ok(SessionListSelection::Close),
                    SessionListSelection::Switch(name) => {
                        return Ok(SessionListSelection::Switch(name));
                    }
                    SessionListSelection::Error(error) => {
                        status = error.clone();
                        render_session_list(stdout, &rows, selected, &status, term_size)?;
                    }
                }
            }
        }
        screen.force_redraw();
    }
}

async fn read_session_list_action() -> anyhow::Result<SessionListAction> {
    tokio::task::spawn_blocking(|| {
        loop {
            match crossterm::event::poll(Duration::from_millis(50)) {
                Ok(true) => match crossterm::event::read() {
                    Ok(Event::Key(key_event))
                        if matches!(key_event.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                    {
                        match key_event.code {
                            KeyCode::Up | KeyCode::Char('k') => {
                                return Ok(SessionListAction::MoveUp);
                            }
                            KeyCode::Down | KeyCode::Char('j') => {
                                return Ok(SessionListAction::MoveDown);
                            }
                            KeyCode::Enter => return Ok(SessionListAction::Select),
                            KeyCode::Esc | KeyCode::Char('q') => {
                                return Ok(SessionListAction::Cancel);
                            }
                            _ => {}
                        }
                    }
                    Ok(_) => {}
                    Err(e) => return Err(anyhow::Error::new(e)),
                },
                Ok(false) => {}
                Err(e) => return Err(anyhow::Error::new(e)),
            }
        }
    })
    .await?
}

fn render_session_list<W: Write>(
    out: &mut W,
    rows: &[SessionListRow],
    selected: usize,
    status: &str,
    term_size: (u16, u16),
) -> std::io::Result<()> {
    let (cols, rows_count) = term_size;
    write!(out, "\x1b[?2026h\x1b[2J\x1b[H")?;
    writeln!(out, "qscreen sessions")?;
    writeln!(out, "Use Up/Down or k/j, Enter to switch, Esc/q to cancel")?;
    writeln!(out)?;

    if rows.is_empty() {
        writeln!(out, "  no sessions")?;
    } else {
        for (idx, row) in rows.iter().enumerate() {
            let selector = if idx == selected { ">" } else { " " };
            let current = if row.is_current { "*" } else { " " };
            writeln!(
                out,
                "{} {} {:<24} {:<14} {:>8}",
                selector,
                current,
                truncate_for_terminal(&row.name, 24),
                truncate_for_terminal(&row.state, 14),
                truncate_for_terminal(&row.size, 8)
            )?;
        }
    }

    let used_lines = rows.len() as u16 + 4;
    if rows_count > used_lines + 1 {
        write!(out, "\x1b[{};1H", rows_count)?;
    } else {
        writeln!(out)?;
    }
    let mut status_line = if status.is_empty() {
        "* marks current session".to_string()
    } else {
        status.to_string()
    };
    status_line = truncate_for_terminal(&status_line, cols as usize);
    write!(out, "{}\x1b[?2026l", status_line)?;
    out.flush()
}

fn truncate_for_terminal(value: &str, width: usize) -> String {
    value.chars().take(width).collect()
}

fn is_prefix_key_event(event: crossterm::event::KeyEvent, prefix: PrefixKey) -> bool {
    if event.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(event.code, KeyCode::Char(c) if c.eq_ignore_ascii_case(&prefix.ctrl_char))
    {
        return true;
    }

    key_event_to_bytes(event) == [prefix.byte]
}

fn key_char_eq_ignore_ascii_case(code: KeyCode, expected: char) -> bool {
    matches!(code, KeyCode::Char(c) if c.eq_ignore_ascii_case(&expected))
}

fn session_list_action_enabled() -> bool {
    true
}

// ── 连接工具 ──────────────────────────────────────────────────────────────────

struct TcpConn {
    #[cfg(windows)]
    stream: tokio::net::windows::named_pipe::NamedPipeClient,
    #[cfg(not(windows))]
    stream: tokio::net::UnixStream,
}

#[cfg(windows)]
async fn connect() -> anyhow::Result<TcpConn> {
    use tokio::net::windows::named_pipe::ClientOptions;
    let pipe = pipe_name();
    let stream = ClientOptions::new()
        .open(&pipe)
        .with_context(|| format!("connect to daemon pipe {}", pipe))?;
    Ok(TcpConn { stream })
}

#[cfg(unix)]
async fn connect() -> anyhow::Result<TcpConn> {
    let path = pipe_name();
    let stream = tokio::net::UnixStream::connect(&path)
        .await
        .with_context(|| format!("connect to daemon socket {}", path))?;
    Ok(TcpConn { stream })
}

async fn ensure_and_connect() -> anyhow::Result<TcpConn> {
    if let Ok(conn) = connect().await {
        return Ok(conn);
    }

    spawn_daemon().context("spawn daemon")?;

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if let Ok(conn) = connect().await {
            return Ok(conn);
        }
        if std::time::Instant::now() > deadline {
            anyhow::bail!("daemon did not start within 5 seconds");
        }
    }
}

fn spawn_daemon() -> anyhow::Result<()> {
    let exe = std::env::current_exe().context("get current exe")?;

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        std::process::Command::new(&exe)
            .arg("--daemon")
            .creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS)
            .spawn()
            .context("spawn daemon process")?;
    }

    #[cfg(not(windows))]
    {
        use std::process::Stdio;
        std::process::Command::new(&exe)
            .arg("--daemon")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("spawn daemon process")?;
    }

    Ok(())
}

// ── 协议辅助 ──────────────────────────────────────────────────────────────────

async fn send_msg(conn: &mut TcpConn, msg: Message) -> anyhow::Result<()> {
    let bytes = msg.to_json_line()?;
    conn.stream.write_all(&bytes).await?;
    Ok(())
}

async fn recv_msg(conn: &mut TcpConn) -> anyhow::Result<Message> {
    let mut reader = BufReader::new(&mut conn.stream);
    let mut line = String::new();
    let n = reader.read_line(&mut line).await?;
    if n == 0 {
        anyhow::bail!("connection closed");
    }
    Message::from_json(&line).context("parse response")
}

async fn send_recv_ok(conn: &mut TcpConn, msg: Message) -> anyhow::Result<()> {
    let id = msg.id.clone();
    send_msg(conn, msg).await?;
    let resp = recv_msg(conn).await?;
    check_response(&resp, &id)
}

fn check_response(resp: &Message, want_id: &str) -> anyhow::Result<()> {
    if resp.kind != MessageKind::Response {
        anyhow::bail!("expected response, got {:?}", resp.kind);
    }
    if !resp.id.is_empty() && resp.id != want_id {
        anyhow::bail!("id mismatch: got {} want {}", resp.id, want_id);
    }
    if !resp.error.is_empty() {
        anyhow::bail!("{}", resp.error);
    }
    Ok(())
}

// ── 终端尺寸 ──────────────────────────────────────────────────────────────────

fn get_terminal_size() -> anyhow::Result<(u16, u16)> {
    let (w, h) = crossterm::terminal::size()?;
    Ok((w, h))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_prefix_accepts_supported_aliases() {
        assert_eq!(PrefixKey::parse("C-a").unwrap(), DEFAULT_PREFIX);
        assert_eq!(PrefixKey::parse("c-a").unwrap(), DEFAULT_PREFIX);
        assert_eq!(PrefixKey::parse("Ctrl+A").unwrap(), DEFAULT_PREFIX);
        assert_eq!(
            PrefixKey::parse("ctrl+z").unwrap(),
            PrefixKey {
                ctrl_char: 'Z',
                byte: 0x1a,
            }
        );
    }

    #[test]
    fn parse_prefix_generates_boundary_control_bytes() {
        assert_eq!(PrefixKey::parse("Ctrl+A").unwrap().byte, 0x01);
        assert_eq!(PrefixKey::parse("Ctrl+Z").unwrap().byte, 0x1a);
    }

    #[test]
    fn parse_prefix_rejects_invalid_values() {
        for value in ["", "Alt+A", "Ctrl+1", "Ctrl+AA", "C-", "C-ab", "A"] {
            let err = PrefixKey::parse(value).unwrap_err().to_string();
            assert!(err.starts_with("invalid prefix"), "{value}: {err}");
        }
    }

    #[test]
    fn client_config_uses_default_prefix() {
        let (config, args) = parse_client_config_with_env(vec![], None).unwrap();
        assert_eq!(config.prefix, DEFAULT_PREFIX);
        assert!(args.is_empty());
    }

    #[test]
    fn client_config_uses_environment_fallback() {
        let (config, args) =
            parse_client_config_with_env(vec!["attach".into(), "work".into()], Some("C-b".into()))
                .unwrap();
        assert_eq!(
            config.prefix,
            PrefixKey {
                ctrl_char: 'B',
                byte: 0x02,
            }
        );
        assert_eq!(args, vec!["attach", "work"]);
    }

    #[test]
    fn client_config_cli_overrides_environment() {
        let (config, args) = parse_client_config_with_env(
            vec![
                "--prefix".into(),
                "C-b".into(),
                "attach".into(),
                "work".into(),
            ],
            Some("C-a".into()),
        )
        .unwrap();
        assert_eq!(
            config.prefix,
            PrefixKey {
                ctrl_char: 'B',
                byte: 0x02,
            }
        );
        assert_eq!(args, vec!["attach", "work"]);
    }

    #[test]
    fn client_config_accepts_prefix_for_all_entry_shapes() {
        for args in [
            vec!["--prefix", "C-b", "attach", "work"],
            vec!["--prefix", "C-b", "new", "work"],
            vec!["--prefix", "C-b"],
        ] {
            let (config, remaining) =
                parse_client_config_with_env(args.iter().map(|s| s.to_string()).collect(), None)
                    .unwrap();
            assert_eq!(config.prefix.ctrl_char, 'B');
            assert!(
                !remaining
                    .iter()
                    .any(|arg| arg == "--prefix" || arg == "C-b")
            );
        }
    }

    #[test]
    fn client_config_rejects_invalid_prefix_early() {
        let err = parse_client_config_with_env(
            vec![
                "--prefix".into(),
                "Alt+A".into(),
                "attach".into(),
                "work".into(),
            ],
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(err.starts_with("invalid prefix"), "{err}");

        let err =
            parse_client_config_with_env(vec!["attach".into(), "work".into()], Some("C-1".into()))
                .unwrap_err()
                .to_string();
        assert!(err.starts_with("invalid prefix"), "{err}");
    }

    fn ctrl_key(c: char) -> crossterm::event::KeyEvent {
        crossterm::event::KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn char_key(c: char) -> crossterm::event::KeyEvent {
        crossterm::event::KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn raw_char_key(byte: u8) -> crossterm::event::KeyEvent {
        crossterm::event::KeyEvent::new(KeyCode::Char(byte as char), KeyModifiers::NONE)
    }

    fn session(name: &str, attached: bool, exited: bool, width: u32, height: u32) -> SessionInfo {
        SessionInfo {
            name: name.to_string(),
            attached,
            exited,
            exit_code: if exited { 7 } else { 0 },
            created_at: chrono::DateTime::default(),
            width,
            height,
            size: String::new(),
        }
    }

    #[test]
    fn prefix_key_event_matches_default_ctrl_a() {
        assert!(is_prefix_key_event(ctrl_key('a'), DEFAULT_PREFIX));
        assert!(is_prefix_key_event(ctrl_key('A'), DEFAULT_PREFIX));
        assert!(!is_prefix_key_event(ctrl_key('b'), DEFAULT_PREFIX));
    }

    #[test]
    fn prefix_key_event_matches_custom_ctrl_b() {
        let prefix = PrefixKey::parse("C-b").unwrap();
        assert!(is_prefix_key_event(ctrl_key('b'), prefix));
        assert!(is_prefix_key_event(ctrl_key('B'), prefix));
        assert!(!is_prefix_key_event(ctrl_key('a'), prefix));
    }

    #[test]
    fn prefix_key_event_matches_raw_control_bytes() {
        let prefix = PrefixKey::parse("C-b").unwrap();
        assert!(is_prefix_key_event(raw_char_key(0x02), prefix));
        assert!(!is_prefix_key_event(raw_char_key(0x01), prefix));
    }

    #[test]
    fn prefix_state_detaches_for_default_and_custom_prefixes() {
        for prefix in [DEFAULT_PREFIX, PrefixKey::parse("C-b").unwrap()] {
            let mut state = PrefixState::default();
            assert_eq!(state.handle_key(ctrl_key(prefix.ctrl_char), prefix), vec![]);
            assert_eq!(
                state.handle_key(char_key('d'), prefix),
                vec![AttachAction::Detach]
            );
        }
    }

    #[test]
    fn prefix_state_accepts_uppercase_actions() {
        let mut state = PrefixState::default();
        assert_eq!(state.handle_key(ctrl_key('a'), DEFAULT_PREFIX), vec![]);
        assert_eq!(
            state.handle_key(char_key('D'), DEFAULT_PREFIX),
            vec![AttachAction::Detach]
        );

        let mut state = PrefixState::default();
        assert_eq!(state.handle_key(ctrl_key('a'), DEFAULT_PREFIX), vec![]);
        assert_eq!(
            state.handle_key(char_key('S'), DEFAULT_PREFIX),
            vec![AttachAction::OpenSessionList]
        );
    }

    #[test]
    fn prefix_state_sends_literal_prefix_for_double_prefix() {
        for prefix in [DEFAULT_PREFIX, PrefixKey::parse("C-b").unwrap()] {
            let mut state = PrefixState::default();
            assert_eq!(state.handle_key(ctrl_key(prefix.ctrl_char), prefix), vec![]);
            assert_eq!(
                state.handle_key(ctrl_key(prefix.ctrl_char), prefix),
                vec![AttachAction::Input(vec![prefix.byte])]
            );
        }
    }

    #[test]
    fn prefix_state_falls_back_to_literal_prefix_then_normal_byte() {
        for prefix in [DEFAULT_PREFIX, PrefixKey::parse("C-b").unwrap()] {
            let mut state = PrefixState::default();
            assert_eq!(state.handle_key(ctrl_key(prefix.ctrl_char), prefix), vec![]);
            assert_eq!(
                state.handle_key(char_key('x'), prefix),
                vec![
                    AttachAction::Input(vec![prefix.byte]),
                    AttachAction::Input(vec![b'x'])
                ]
            );
        }
    }

    #[test]
    fn prefix_state_treats_detach_key_as_normal_without_pending_prefix() {
        let mut state = PrefixState::default();
        assert_eq!(
            state.handle_key(char_key('d'), DEFAULT_PREFIX),
            vec![AttachAction::Input(vec![b'd'])]
        );
    }

    #[test]
    fn prefix_state_opens_session_list_for_default_and_custom_prefixes() {
        for prefix in [DEFAULT_PREFIX, PrefixKey::parse("C-b").unwrap()] {
            let mut state = PrefixState::default();
            assert_eq!(state.handle_key(ctrl_key(prefix.ctrl_char), prefix), vec![]);
            assert_eq!(
                state.handle_key(char_key('s'), prefix),
                vec![AttachAction::OpenSessionList]
            );
        }
    }

    #[test]
    fn prefix_state_uses_raw_custom_prefix_bytes() {
        let prefix = PrefixKey::parse("C-b").unwrap();

        let mut state = PrefixState::default();
        assert_eq!(state.handle_key(raw_char_key(0x02), prefix), vec![]);
        assert_eq!(
            state.handle_key(char_key('d'), prefix),
            vec![AttachAction::Detach]
        );

        let mut state = PrefixState::default();
        assert_eq!(state.handle_key(raw_char_key(0x02), prefix), vec![]);
        assert_eq!(
            state.handle_key(char_key('s'), prefix),
            vec![AttachAction::OpenSessionList]
        );
    }

    #[test]
    fn session_list_rows_sort_like_list_output_and_mark_current() {
        let sessions = vec![
            session("zeta", false, false, 100, 40),
            session("alpha", true, false, 80, 24),
            session("mid", false, true, 120, 30),
        ];
        let rows = build_session_list_rows(&sessions, "alpha");

        assert_eq!(
            rows.iter().map(|row| row.name.as_str()).collect::<Vec<_>>(),
            vec!["alpha", "mid", "zeta"]
        );
        assert!(rows[0].is_current);
        assert_eq!(rows[0].state, "attached");
        assert_eq!(rows[0].size, "80x24");
        assert_eq!(rows[1].state, "exited(7)");
        assert_eq!(rows[2].state, "detached");
    }

    #[test]
    fn session_list_rows_prefer_protocol_size_label() {
        let mut info = session("work", false, false, 80, 24);
        info.size = "132x43".to_string();
        let rows = build_session_list_rows(&[info], "main");

        assert_eq!(rows[0].size, "132x43");
    }

    #[test]
    fn selection_rules_cover_current_exited_attached_and_attachable() {
        let rows = build_session_list_rows(
            &[
                session("current", true, false, 80, 24),
                session("done", false, true, 80, 24),
                session("busy", true, false, 80, 24),
                session("next", false, false, 80, 24),
            ],
            "current",
        );

        assert_eq!(
            selection_for_session_row(rows.iter().find(|row| row.name == "current").unwrap()),
            SessionListSelection::Close
        );
        assert_eq!(
            selection_for_session_row(rows.iter().find(|row| row.name == "done").unwrap()),
            SessionListSelection::Error(exited_session_error("done"))
        );
        assert_eq!(
            selection_for_session_row(rows.iter().find(|row| row.name == "busy").unwrap()),
            SessionListSelection::Error(attached_session_error("busy"))
        );
        assert_eq!(
            selection_for_session_row(rows.iter().find(|row| row.name == "next").unwrap()),
            SessionListSelection::Switch("next".to_string())
        );
    }

    #[test]
    fn session_list_navigation_clamps_to_bounds() {
        assert_eq!(move_session_list_selection(0, 4, -1), 0);
        assert_eq!(move_session_list_selection(1, 4, -1), 0);
        assert_eq!(move_session_list_selection(2, 4, 1), 3);
        assert_eq!(move_session_list_selection(3, 4, 1), 3);
        assert_eq!(move_session_list_selection(0, 0, 1), 0);
    }

    #[test]
    fn render_session_list_shows_current_marker_selection_and_status() {
        let rows = build_session_list_rows(
            &[
                session("main", true, false, 80, 24),
                session("work", false, false, 100, 30),
            ],
            "main",
        );
        let mut out = Vec::new();

        render_session_list(&mut out, &rows, 1, "ready", (80, 24)).unwrap();

        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("qscreen sessions"));
        assert!(text.contains("  * main"));
        assert!(text.contains(">   work"));
        assert!(text.contains("ready"));
    }

    #[test]
    fn attach_loop_target_helper_retries_only_on_switch() {
        assert_eq!(
            next_attach_target_after_outcome(AttachOutcome::SwitchTo("next".to_string())),
            Some("next".to_string())
        );
        assert_eq!(
            next_attach_target_after_outcome(AttachOutcome::Detached),
            None
        );
        assert_eq!(next_attach_target_after_outcome(AttachOutcome::Ended), None);
    }
}
