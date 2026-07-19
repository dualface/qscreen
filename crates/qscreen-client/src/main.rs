use std::collections::VecDeque;
use std::io::Write;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Context;
use crossterm::event::{
    Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use qscreen_protocol::{
    Command, EventType, FrameMouseEncoding, FrameMouseMode, Message, MessageKind, SessionInfo,
    exited_session_error, validate_session_id, validate_session_name,
};
use qscreen_shared::{daemon_log_path, pipe_name};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

mod color;
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

// ── Entry ────────────────────────────────────────────────────────────────────

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // --daemon mode: start the daemon server
    if args.first().map(|s| s.as_str()) == Some("--daemon") {
        run_daemon_mode();
        return;
    }

    // CLI client mode
    if let Err(e) = run_client(args) {
        eprintln!("{}", color::paint_err(&e.to_string(), color::sgr::ERROR));
        std::process::exit(1);
    }
}

// ── Daemon mode ──────────────────────────────────────────────────────────────

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

// ── Client mode ──────────────────────────────────────────────────────────────

fn run_client(args: Vec<String>) -> anyhow::Result<()> {
    // On every qscn run, first check whether the environment supports color and record the result.
    color::init_and_record();
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
            [cmd, rest @ ..] if cmd == "new" => {
                let opts = parse_new_options(rest)?;
                cmd_new(
                    opts.name.as_deref(),
                    opts.shell.as_deref(),
                    opts.cwd.as_deref(),
                    config,
                )
                .await
            }
            [cmd, session_id] if cmd == "attach" || cmd == "-r" => {
                cmd_attach(session_id, config).await
            }
            [cmd, session_id] if cmd == "kill" => cmd_kill(session_id).await,
            [cmd, session_id, name] if cmd == "rename" => cmd_rename(session_id, name).await,
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

#[derive(Debug)]
struct NewOptions {
    name: Option<String>,
    shell: Option<String>,
    cwd: Option<String>,
}

fn parse_new_options(args: &[String]) -> anyhow::Result<NewOptions> {
    let mut name = None;
    let mut shell = None;
    let mut cwd = None;
    let mut i = 0;

    while i < args.len() {
        match args[i].as_str() {
            "--name" => {
                i += 1;
                let value = args
                    .get(i)
                    .with_context(|| missing_option_value("--name"))?;
                set_once(&mut name, value.clone(), "--name")?;
            }
            "--shell" => {
                i += 1;
                let value = args
                    .get(i)
                    .with_context(|| missing_option_value("--shell"))?;
                set_once(&mut shell, value.clone(), "--shell")?;
            }
            "--cwd" => {
                i += 1;
                let value = args.get(i).with_context(|| missing_option_value("--cwd"))?;
                set_once(&mut cwd, value.clone(), "--cwd")?;
            }
            value if value.starts_with("--name=") => {
                let value = value.trim_start_matches("--name=");
                if value.is_empty() {
                    anyhow::bail!("{}", missing_option_value("--name"));
                }
                set_once(&mut name, value.to_string(), "--name")?;
            }
            value if value.starts_with("--shell=") => {
                let value = value.trim_start_matches("--shell=");
                if value.is_empty() {
                    anyhow::bail!("{}", missing_option_value("--shell"));
                }
                set_once(&mut shell, value.to_string(), "--shell")?;
            }
            value if value.starts_with("--cwd=") => {
                let value = value.trim_start_matches("--cwd=");
                if value.is_empty() {
                    anyhow::bail!("{}", missing_option_value("--cwd"));
                }
                set_once(&mut cwd, value.to_string(), "--cwd")?;
            }
            value if value.starts_with('-') => {
                anyhow::bail!("unknown new option: {value}");
            }
            value => {
                anyhow::bail!("unexpected argument for new: {value}. Use --name <name>");
            }
        }
        i += 1;
    }

    Ok(NewOptions { name, shell, cwd })
}

fn set_once(slot: &mut Option<String>, value: String, label: &str) -> anyhow::Result<()> {
    if slot.is_some() {
        anyhow::bail!("duplicate {label}");
    }
    *slot = Some(value);
    Ok(())
}

fn missing_option_value(option: &str) -> String {
    format!("missing value for {option}")
}

// ── Language detection ───────────────────────────────────────────────────────

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
        windows_locale_is_chinese()
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

// ── Help text ────────────────────────────────────────────────────────────────

fn print_help() {
    let raw = if is_chinese() {
        help_text_zh()
    } else {
        help_text_en()
    };
    print!("{}", colorize_help(raw));
}

/// Colorize the help text: the first-line title and section headings of the
/// form `Xxx:` use bold cyan, everything else stays as-is. When stdout lacks
/// color support, [`color::paint`] returns text unchanged, matching the
/// uncolored version.
fn colorize_help(raw: &str) -> String {
    let mut out = String::new();
    for (idx, line) in raw.lines().enumerate() {
        let is_title = idx == 0 && !line.trim().is_empty();
        let is_section = !line.starts_with(char::is_whitespace)
            && !line.trim().is_empty()
            && line.trim_end().ends_with(':');
        if is_title || is_section {
            out.push_str(&color::paint(line, color::sgr::HEADER));
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}

fn help_text_zh() -> &'static str {
    r#"qscreen — 轻量终端会话管理器

用法:
  qscn [--prefix C-a]          智能启动：无会话时新建并进入，单会话时直接 attach，
                            多会话时列出所有会话
  qscn [--prefix C-a] new
                               新建自动命名会话并进入
  qscn [--prefix C-a] new --name <name>
                               用参数指定显示名
  qscn [--prefix C-a] new --shell <shell>
                               指定启动 shell（Windows: cmd、powershell 或可执行文件路径；Unix: shell 路径）
  qscn [--prefix C-a] new --cwd <path>
                               指定会话启动工作目录
  qscn [--prefix C-a] attach <session_id>
                               进入已有会话
  qscn [--prefix C-a] -r <session_id>
                               同 attach，兼容 tmux 风格
  qscn ls                      列出所有会话（同 list）
  qscn list                    列出所有会话
  qscn kill <session_id>       强制终止指定会话
  qscn rename <session_id> <name>
                               修改会话显示名
  qscn shutdown                停止后台 daemon（所有会话将被关闭）
  qscn -h, --help              显示此帮助

前缀:
  --prefix C-b                 使用 Ctrl+B 作为当前命令的会话前缀
  QSCREEN_PREFIX=C-b           为所有命令设置备用前缀
  支持 C-a..C-z 或 Ctrl+A..Ctrl+Z；CLI 参数优先于环境变量

会话内热键:
  <prefix> d                  从当前会话 detach（会话继续在后台运行）
  <prefix> <prefix>           向 PTY 发送字面前缀字符
  <prefix> s                  打开会话列表（Enter 切换，c 新建，r 改名，q 取消）

ls 输出格式:
  <session_id>  <name>  <状态>  <创建时间>  <终端尺寸>
  状态: attached | detached | exited(<退出码>)

示例:
  qscn                         # 自动进入唯一会话，或新建自动命名会话
  qscn new                     # 新建自动分配 session_id 的会话
  qscn new --name work         # 新建显示名为 work 的会话
  qscn new --shell cmd --name work
  qscn new --cwd C:\work --name work
  qscn --prefix C-b attach 1   # 使用 Ctrl+B 作为前缀进入 session_id=1
  qscn attach 1                # 重新进入 session_id=1
  qscn rename 1 work           # 修改 session_id=1 的显示名
  qscn ls                      # 查看所有会话状态
  qscn kill 1                  # 终止 session_id=1
"#
}

fn help_text_en() -> &'static str {
    r#"qscreen — lightweight terminal session manager

Usage:
  qscn [--prefix C-a]          smart launch: create and enter a session if no sessions,
                            attach if one session, list all if multiple
  qscn [--prefix C-a] new
                               create an auto-named session and attach
  qscn [--prefix C-a] new --name <name>
                               specify the display name as an option
  qscn [--prefix C-a] new --shell <shell>
                               specify the startup shell (Windows: cmd, powershell, or an executable path; Unix: shell path)
  qscn [--prefix C-a] new --cwd <path>
                               specify the session working directory
  qscn [--prefix C-a] attach <session_id>
                               attach to an existing session
  qscn [--prefix C-a] -r <session_id>
                               same as attach (tmux-style shorthand)
  qscn ls                      list all sessions (alias: list)
  qscn list                    list all sessions
  qscn kill <session_id>       forcibly terminate a session
  qscn rename <session_id> <name>
                               change a session display name
  qscn shutdown                stop the background daemon (closes all sessions)
  qscn -h, --help              show this help

Prefix:
  --prefix C-b                 use Ctrl+B as the session prefix for this command
  QSCREEN_PREFIX=C-b           set a fallback prefix for every command
  Values: C-a..C-z or Ctrl+A..Ctrl+Z; CLI takes precedence over env

Key bindings (inside a session):
  <prefix> d                  detach from session (session keeps running)
  <prefix> <prefix>           send a literal prefix key to the PTY
  <prefix> s                  open the session list (Enter switch, c create, r rename, q cancel)

ls output format:
  <session_id>  <name>  <state>  <created-at>  <terminal-size>
  states: attached | detached | exited(<code>)

Examples:
  qscn                         # auto-attach or create an auto-named session
  qscn new                     # create a session with an auto-assigned session_id
  qscn new --name work         # create a session with display name 'work'
  qscn new --shell cmd --name work
  qscn new --cwd C:\work --name work
  qscn --prefix C-b attach 1   # attach to session_id=1 using Ctrl+B as the prefix
  qscn attach 1                # reattach to session_id=1
  qscn rename 1 work           # change the display name for session_id=1
  qscn ls                      # show all session states
  qscn kill 1                  # terminate session_id=1
"#
}

// ── Subcommand implementations ───────────────────────────────────────────────

async fn cmd_default(config: ClientConfig) -> anyhow::Result<()> {
    let sessions = list_sessions().await?;
    match sessions.len() {
        0 => cmd_new_and_attach("", None, None, config).await,
        1 => cmd_attach(&sessions[0].session_id.clone(), config).await,
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

async fn cmd_new(
    name: Option<&str>,
    shell: Option<&str>,
    cwd: Option<&str>,
    config: ClientConfig,
) -> anyhow::Result<()> {
    cmd_new_and_attach(name.unwrap_or_default(), shell, cwd, config).await
}

async fn cmd_new_and_attach(
    name: &str,
    shell: Option<&str>,
    cwd: Option<&str>,
    config: ClientConfig,
) -> anyhow::Result<()> {
    // Run preflight before creating the session, so a failed preflight does not leave an un-attachable orphan session.
    term::preflight_interactive()?;
    if !name.is_empty() {
        validate_session_name(name)?;
    }
    let session_id = create_session_with_options(name, shell, cwd).await?;
    attach_session_loop(&session_id, config).await
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CwdRequest {
    cwd: String,
    cwd_bytes: Vec<u8>,
}

async fn create_session_with_options(
    name: &str,
    shell: Option<&str>,
    cwd: Option<&str>,
) -> anyhow::Result<String> {
    let requested_cwd = cwd_for_request(cwd)?;
    let mut conn = ensure_and_connect().await?;
    let resp = send_recv_ok(
        &mut conn,
        Message {
            kind: MessageKind::Request,
            id: "1".to_string(),
            command: Some(Command::New),
            name: name.to_string(),
            shell: shell.unwrap_or_default().to_string(),
            cwd: requested_cwd.cwd.clone(),
            cwd_bytes: requested_cwd.cwd_bytes.clone(),
            ..Default::default()
        },
    )
    .await?;
    validate_session_id(&resp.session_id)?;
    if !cwd_acknowledged(&requested_cwd, &resp) {
        let session_id = resp.session_id.clone();
        let _ = send_recv_ok(
            &mut conn,
            Message {
                kind: MessageKind::Request,
                id: "2".to_string(),
                command: Some(Command::Kill),
                session_id,
                ..Default::default()
            },
        )
        .await;
        anyhow::bail!("daemon does not support --cwd; restart qscn daemon and retry");
    }
    Ok(resp.session_id)
}

fn cwd_acknowledged(requested: &CwdRequest, acknowledged: &Message) -> bool {
    if requested.cwd_bytes.is_empty() {
        requested.cwd == acknowledged.cwd
    } else {
        requested.cwd.is_empty()
            && acknowledged.cwd.is_empty()
            && requested.cwd_bytes == acknowledged.cwd_bytes
    }
}

fn cwd_for_request(cwd: Option<&str>) -> anyhow::Result<CwdRequest> {
    let path = if let Some(cwd) = cwd.filter(|value| !value.is_empty()) {
        let path = std::path::Path::new(cwd);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .context("resolve current directory for --cwd")?
                .join(path)
        }
    } else {
        // When --cwd is not given explicitly, inherit the client's (parent's)
        // current working directory; otherwise the session would land in the
        // resident daemon's startup directory rather than where the user ran `qscn new`.
        std::env::current_dir().context("resolve current directory for session cwd")?
    };
    cwd_request_from_path(&path)
}

fn cwd_request_from_path(path: &std::path::Path) -> anyhow::Result<CwdRequest> {
    if let Some(cwd) = path.to_str() {
        return Ok(CwdRequest {
            cwd: cwd.to_string(),
            cwd_bytes: Vec::new(),
        });
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;

        Ok(CwdRequest {
            cwd: String::new(),
            cwd_bytes: path.as_os_str().as_bytes().to_vec(),
        })
    }
    #[cfg(not(unix))]
    {
        anyhow::bail!("current directory is not valid UTF-8")
    }
}

async fn cmd_attach(session_id: &str, config: ClientConfig) -> anyhow::Result<()> {
    term::preflight_interactive()?;
    validate_session_id(session_id)?;
    attach_session_loop(session_id, config).await
}

async fn cmd_kill(session_id: &str) -> anyhow::Result<()> {
    validate_session_id(session_id)?;
    let mut conn = ensure_and_connect().await?;
    send_recv_ok(
        &mut conn,
        Message {
            kind: MessageKind::Request,
            id: "1".to_string(),
            command: Some(Command::Kill),
            session_id: session_id.to_string(),
            ..Default::default()
        },
    )
    .await?;
    Ok(())
}

async fn cmd_rename(session_id: &str, name: &str) -> anyhow::Result<()> {
    validate_session_id(session_id)?;
    validate_session_name(name)?;
    let mut conn = ensure_and_connect().await?;
    send_recv_ok(
        &mut conn,
        Message {
            kind: MessageKind::Request,
            id: "1".to_string(),
            command: Some(Command::Rename),
            session_id: session_id.to_string(),
            name: name.to_string(),
            ..Default::default()
        },
    )
    .await?;
    Ok(())
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

/// Create a new session via the daemon using the default name/shell, returning its session_id.
/// When `cwd` is `Some(non-empty)`, the new session inherits that directory; otherwise it falls back to the client's current directory.
async fn create_session_in(cwd: Option<&str>) -> anyhow::Result<String> {
    let cwd = cwd.filter(|value| !value.is_empty());
    create_session_with_options("", None, cwd).await
}

/// Rename the given session. The name is validated first, then a Rename request is sent.
async fn rename_session(session_id: &str, name: &str) -> anyhow::Result<()> {
    validate_session_name(name)?;
    let mut conn = ensure_and_connect().await?;
    send_recv_ok(
        &mut conn,
        Message {
            kind: MessageKind::Request,
            id: "1".to_string(),
            command: Some(Command::Rename),
            session_id: session_id.to_string(),
            name: name.to_string(),
            ..Default::default()
        },
    )
    .await?;
    Ok(())
}

fn print_sessions(sessions: &[SessionInfo]) {
    for s in sessions {
        if color::supported() {
            println!("{}", colored_session_line(s));
        } else {
            println!("{}", format_session_line(s));
        }
    }
}

fn session_created_label(s: &SessionInfo) -> String {
    if s.created_at.timestamp() == 0 {
        "-".to_string()
    } else {
        s.created_at.format("%Y-%m-%dT%H:%M:%SZ").to_string()
    }
}

fn format_session_line(s: &SessionInfo) -> String {
    format!(
        "{}\t{}\t{}\t{}\t{}\t{}",
        s.session_id,
        s.name,
        session_state_label(s),
        session_created_label(s),
        session_size_label(s),
        session_cwd_label(s)
    )
}

/// Same column layout as [`format_session_line`], but colored per column (called only when stdout supports color).
fn colored_session_line(s: &SessionInfo) -> String {
    format!(
        "{}\t{}\t{}\t{}\t{}\t{}",
        color::paint(&s.session_id, color::sgr::ID),
        color::paint(&s.name, color::sgr::NAME),
        color::paint(
            &session_state_label(s),
            color::state_sgr(s.exited, s.attached)
        ),
        color::paint(&session_created_label(s), color::sgr::CREATED),
        color::paint(&session_size_label(s), color::sgr::SIZE),
        color::paint(&session_cwd_label(s), color::sgr::CWD)
    )
}

fn session_cwd_label(session: &SessionInfo) -> String {
    if session.cwd.is_empty() {
        "-".to_string()
    } else {
        escape_terminal_controls(&session.cwd)
    }
}

fn escape_terminal_controls(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        if ch.is_control() {
            escaped.extend(ch.escape_default());
        } else {
            escaped.push(ch);
        }
    }
    escaped
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionListRow {
    session_id: String,
    name: String,
    state: String,
    size: String,
    cwd: String,
    /// Raw, unescaped working directory, used so "create a session from the list" can inherit its cwd.
    cwd_raw: String,
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

fn build_session_list_rows(
    sessions: &[SessionInfo],
    current_session_id: &str,
) -> Vec<SessionListRow> {
    let mut rows: Vec<SessionListRow> = sessions
        .iter()
        .map(|session| SessionListRow {
            session_id: session.session_id.clone(),
            name: session.name.clone(),
            state: session_state_label(session),
            size: session_size_label(session),
            cwd: session_cwd_label(session),
            cwd_raw: session.cwd.clone(),
            is_current: session.session_id == current_session_id,
            exited: session.exited,
            attached: session.attached,
        })
        .collect();
    rows.sort_by_key(|row| row.session_id.parse::<u64>().unwrap_or(u64::MAX));
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
        SessionListSelection::Error(exited_session_error(&row.session_id))
    } else {
        SessionListSelection::Switch(row.session_id.clone())
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

// ── Attach implementation ────────────────────────────────────────────────────

async fn attach_session_loop(initial_session_id: &str, config: ClientConfig) -> anyhow::Result<()> {
    let mut session_id = initial_session_id.to_string();

    loop {
        validate_session_id(&session_id)?;
        let outcome = attach_session_once(&session_id, config).await?;
        match next_attach_target_after_outcome(outcome) {
            Some(next_session_id) => session_id = next_session_id,
            None => return Ok(()),
        }
    }
}

fn next_attach_target_after_outcome(outcome: AttachOutcome) -> Option<String> {
    match outcome {
        AttachOutcome::SwitchTo(next_session_id) => Some(next_session_id),
        AttachOutcome::Detached | AttachOutcome::Ended => None,
    }
}

async fn attach_session_once(
    session_id: &str,
    config: ClientConfig,
) -> anyhow::Result<AttachOutcome> {
    let mut conn = ensure_and_connect().await?;
    let term_size = get_terminal_size().unwrap_or((80, 24));
    let (term_width, term_height) = term_size;

    let attach_id = "1";
    send_msg(
        &mut conn,
        attach_request_message(attach_id, session_id, term_width, term_height),
    )
    .await?;

    let resp = recv_msg(&mut conn).await?;
    check_response(&resp, attach_id)?;

    let _terminal = TerminalCleanupGuard::enter()?;

    let session_id_owned = session_id.to_string();
    run_attach_loop(conn, session_id_owned, term_size, config.prefix).await
}

fn attach_request_message(
    attach_id: &str,
    session_id: &str,
    term_width: u16,
    term_height: u16,
) -> Message {
    Message {
        kind: MessageKind::Request,
        id: attach_id.to_string(),
        command: Some(Command::Attach),
        session_id: session_id.to_string(),
        width: term_width as u32,
        height: term_height as u32,
        ..Default::default()
    }
}

struct TerminalCleanupGuard;

impl TerminalCleanupGuard {
    fn enter() -> anyhow::Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        let mut stdout = std::io::stdout();
        term::prepare_attach_terminal(&mut stdout)?;

        #[cfg(windows)]
        {
            use std::io::Write;
            let _ = stdout.write_all(b"\x1b[?9001l");
            let _ = stdout.flush();
        }

        Ok(Self)
    }
}

impl Drop for TerminalCleanupGuard {
    fn drop(&mut self) {
        let mut stdout = std::io::stdout();
        let _ = term::cleanup_attach_terminal(&mut stdout);
    }
}

// ── Keyboard event → PTY byte sequence ───────────────────────────────────────

fn key_event_to_bytes(
    event: crossterm::event::KeyEvent,
    input_modes: term::InputModeState,
) -> Vec<u8> {
    let ctrl = event.modifiers.contains(KeyModifiers::CONTROL);
    let alt = event.modifiers.contains(KeyModifiers::ALT);
    let shift = event.modifiers.contains(KeyModifiers::SHIFT);
    let modifier = csi_modifier(event.modifiers);
    let cursor_prefix = if input_modes.application_cursor {
        b'O'
    } else {
        b'['
    };

    match event.code {
        // Backspace → DEL (0x7f): ConPTY translates 0x7f into the Backspace key,
        // while 0x08 is translated into Ctrl+Backspace, which PSReadLine treats as BackwardKillWord and deletes a whole word.
        KeyCode::Backspace if ctrl => vec![0x08],
        KeyCode::Backspace if alt => vec![0x1b, 0x7f],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Tab if shift => b"\x1b[Z".to_vec(),
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Tab => {
            if modifier > 1 {
                modified_csi('u', 9, modifier)
            } else {
                vec![b'\t']
            }
        }
        KeyCode::Esc => vec![0x1b],
        // Delete → CSI 3~: 0x7f would be treated as Backspace by ConPTY
        KeyCode::Delete => tilde_key(3, modifier),

        KeyCode::Up => cursor_key(cursor_prefix, 'A', modifier),
        KeyCode::Down => cursor_key(cursor_prefix, 'B', modifier),
        KeyCode::Right => cursor_key(cursor_prefix, 'C', modifier),
        KeyCode::Left => cursor_key(cursor_prefix, 'D', modifier),
        KeyCode::Home => cursor_key(cursor_prefix, 'H', modifier),
        KeyCode::End => cursor_key(cursor_prefix, 'F', modifier),
        KeyCode::PageUp => tilde_key(5, modifier),
        KeyCode::PageDown => tilde_key(6, modifier),
        KeyCode::Insert => tilde_key(2, modifier),
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

fn csi_modifier(modifiers: KeyModifiers) -> u8 {
    let mut value = 1;
    if modifiers.contains(KeyModifiers::SHIFT) {
        value += 1;
    }
    if modifiers.contains(KeyModifiers::ALT) {
        value += 2;
    }
    if modifiers.contains(KeyModifiers::CONTROL) {
        value += 4;
    }
    value
}

fn cursor_key(prefix: u8, final_byte: char, modifier: u8) -> Vec<u8> {
    if modifier > 1 {
        format!("\x1b[1;{}{}", modifier, final_byte).into_bytes()
    } else {
        vec![0x1b, prefix, final_byte as u8]
    }
}

fn tilde_key(code: u8, modifier: u8) -> Vec<u8> {
    if modifier > 1 {
        format!("\x1b[{};{}~", code, modifier).into_bytes()
    } else {
        format!("\x1b[{}~", code).into_bytes()
    }
}

fn modified_csi(final_byte: char, code: u8, modifier: u8) -> Vec<u8> {
    format!("\x1b[{};{}{}", code, modifier, final_byte).into_bytes()
}

fn mouse_event_to_bytes(event: MouseEvent, input_modes: term::InputModeState) -> Option<Vec<u8>> {
    if input_modes.mouse_mode == FrameMouseMode::None {
        return None;
    }

    let (button_code, final_byte, motion, button_down) = match event.kind {
        MouseEventKind::Down(button) => (mouse_button_code(button), b'M', false, true),
        MouseEventKind::Up(_) => (3, b'm', false, false),
        MouseEventKind::Drag(button) => (mouse_button_code(button) | 32, b'M', true, true),
        MouseEventKind::Moved => (35, b'M', true, false),
        MouseEventKind::ScrollUp => (64, b'M', false, true),
        MouseEventKind::ScrollDown => (65, b'M', false, true),
        MouseEventKind::ScrollLeft => (66, b'M', false, true),
        MouseEventKind::ScrollRight => (67, b'M', false, true),
    };

    if !mouse_mode_reports(input_modes.mouse_mode, button_down, motion) {
        return None;
    }

    let mut code = button_code;
    if event.modifiers.contains(KeyModifiers::SHIFT) {
        code |= 4;
    }
    if event.modifiers.contains(KeyModifiers::ALT) {
        code |= 8;
    }
    if event.modifiers.contains(KeyModifiers::CONTROL) {
        code |= 16;
    }

    let x = event.column.saturating_add(1);
    let y = event.row.saturating_add(1);
    match input_modes.mouse_encoding {
        FrameMouseEncoding::Sgr => {
            Some(format!("\x1b[<{};{};{}{}", code, x, y, final_byte as char).into_bytes())
        }
        FrameMouseEncoding::Default => {
            if x > 223 || y > 223 {
                return None;
            }
            Some(vec![
                0x1b,
                b'[',
                b'M',
                code.saturating_add(32),
                (x as u8).saturating_add(32),
                (y as u8).saturating_add(32),
            ])
        }
        FrameMouseEncoding::Utf8 => {
            let mut out = vec![0x1b, b'[', b'M'];
            push_mouse_utf8(&mut out, u16::from(code))?;
            push_mouse_utf8(&mut out, x)?;
            push_mouse_utf8(&mut out, y)?;
            Some(out)
        }
    }
}

fn mouse_button_code(button: MouseButton) -> u8 {
    match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    }
}

fn push_mouse_utf8(buf: &mut Vec<u8>, value: u16) -> Option<()> {
    let ch = char::from_u32(u32::from(value) + 32)?;
    let mut encoded = [0; 4];
    buf.extend_from_slice(ch.encode_utf8(&mut encoded).as_bytes());
    Some(())
}

fn mouse_mode_reports(mode: FrameMouseMode, pressed: bool, motion: bool) -> bool {
    match mode {
        FrameMouseMode::None => false,
        FrameMouseMode::Press => pressed && !motion,
        FrameMouseMode::PressRelease => !motion,
        FrameMouseMode::ButtonMotion => !motion || pressed,
        FrameMouseMode::AnyMotion => true,
    }
}

// ── Attach main loop ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
enum AttachAction {
    Input(Vec<u8>),
    Resize(u16, u16),
    Focus,
    Detach,
    OpenSessionList,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SessionListAction {
    MoveUp,
    MoveDown,
    Select,
    Create,
    Rename,
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
        input_modes: term::InputModeState,
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
            let bytes = key_event_to_bytes(key_event, input_modes);
            if !bytes.is_empty() {
                actions.push(AttachAction::Input(bytes));
            }
            return actions;
        }

        let bytes = key_event_to_bytes(key_event, input_modes);
        if bytes.is_empty() {
            Vec::new()
        } else {
            vec![AttachAction::Input(bytes)]
        }
    }
}

async fn run_attach_loop(
    conn: TcpConn,
    session_id: String,
    term_size: (u16, u16),
    prefix: PrefixKey,
) -> anyhow::Result<AttachOutcome> {
    let (read_half, write_half) = tokio::io::split(conn.stream);
    let writer = Arc::new(tokio::sync::Mutex::new(write_half));
    let mut reader = BufReader::new(read_half);
    let (msg_tx, mut msg_rx) = tokio::sync::mpsc::unbounded_channel::<Message>();
    tokio::spawn(async move {
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let Ok(msg) = Message::from_json(&line) else {
                break;
            };
            if msg_tx.send(msg).is_err() {
                break;
            }
        }
    });

    let (cols, rows) = term_size;
    let mut screen = term::TermScreen::new(rows, cols);
    let mut frame_renderer = term::FrameRenderer::default();

    let writer_c = writer.clone();
    let session_id_c = session_id.clone();
    let mut msg_id: u64 = 10;

    let (action_tx, mut action_rx) = tokio::sync::mpsc::unbounded_channel::<AttachAction>();
    let stop_input = Arc::new(AtomicBool::new(false));
    let input_modes = Arc::new(std::sync::Mutex::new(frame_renderer.input_modes()));
    let mut input_handle = spawn_attach_input_reader(
        action_tx.clone(),
        stop_input.clone(),
        prefix,
        input_modes.clone(),
    );

    let mut stdout = std::io::stdout();
    let mut pending_messages: VecDeque<Message> = VecDeque::new();

    let outcome = 'attach: loop {
        tokio::select! {
            msg = next_attach_message(&mut pending_messages, &mut msg_rx) => {
                let Some(msg) = msg else {
                    break AttachOutcome::Ended;
                };
                match msg.kind {
                    MessageKind::Event => match msg.event {
                        Some(EventType::Output) => {
                            frame_renderer.reset();
                            let _ = stdout.write_all(&msg.payload);
                            let _ = stdout.flush();
                        }
                        Some(EventType::Frame) => {
                            let mut latest_frame = msg.frame;
                            loop {
                                match msg_rx.try_recv() {
                                    Ok(next_msg) if next_msg.event == Some(EventType::Frame) => {
                                        latest_frame = next_msg.frame;
                                    }
                                    Ok(next_msg) if next_msg.event == Some(EventType::Exit) => {
                                        break 'attach AttachOutcome::Ended;
                                    }
                                    Ok(next_msg) => pending_messages.push_back(next_msg),
                                    Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                                    Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                                        break 'attach AttachOutcome::Ended;
                                    }
                                }
                            }
                            if let Some(frame) = latest_frame.as_ref() {
                                let _ = frame_renderer.render(&mut stdout, frame);
                                *input_modes.lock().unwrap() = frame_renderer.input_modes();
                            }
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
                            session_id: session_id_c.clone(),
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
                        frame_renderer.reset();
                        msg_id += 1;
                        let resize_msg = Message {
                            kind: MessageKind::Request,
                            id: msg_id.to_string(),
                            command: Some(Command::Resize),
                            session_id: session_id_c.clone(),
                            width: w as u32,
                            height: h as u32,
                            ..Default::default()
                        };
                        let bytes = resize_msg.to_json_line()?;
                        if writer_c.lock().await.write_all(&bytes).await.is_err() {
                            break AttachOutcome::Ended;
                        }
                    }
                    Some(AttachAction::Focus) => {
                        msg_id += 1;
                        let focus_msg = Message {
                            kind: MessageKind::Request,
                            id: msg_id.to_string(),
                            command: Some(Command::Focus),
                            session_id: session_id_c.clone(),
                            ..Default::default()
                        };
                        let bytes = focus_msg.to_json_line()?;
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
                            session_id: session_id_c.clone(),
                            ..Default::default()
                        };
                        let bytes = detach_msg.to_json_line()?;
                        let _ = writer_c.lock().await.write_all(&bytes).await;
                        break AttachOutcome::Detached;
                    }
                    Some(AttachAction::OpenSessionList) => {
                        stop_input.store(true, Ordering::Relaxed);
                        let _ = input_handle.await;

                        match run_session_list_mode(&session_id_c, screen.size(), &mut stdout).await? {
                            SessionListSelection::Switch(next_session_id) => {
                                break AttachOutcome::SwitchTo(next_session_id);
                            }
                            SessionListSelection::Close | SessionListSelection::Error(_) => {
                                let (w, h) = screen.size();
                                frame_renderer.reset();
                                msg_id += 1;
                                let resize_msg = Message {
                                    kind: MessageKind::Request,
                                    id: msg_id.to_string(),
                                    command: Some(Command::Resize),
                                    session_id: session_id_c.clone(),
                                    width: w as u32,
                                    height: h as u32,
                                    ..Default::default()
                                };
                                if let Ok(bytes) = resize_msg.to_json_line() {
                                    let _ = writer_c.lock().await.write_all(&bytes).await;
                                }
                                stop_input.store(false, Ordering::Relaxed);
                                input_handle = spawn_attach_input_reader(
                                    action_tx.clone(),
                                    stop_input.clone(),
                                    prefix,
                                    input_modes.clone(),
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

async fn next_attach_message(
    pending_messages: &mut VecDeque<Message>,
    msg_rx: &mut tokio::sync::mpsc::UnboundedReceiver<Message>,
) -> Option<Message> {
    if let Some(msg) = pending_messages.pop_front() {
        return Some(msg);
    }
    msg_rx.recv().await
}

fn spawn_attach_input_reader(
    action_tx: tokio::sync::mpsc::UnboundedSender<AttachAction>,
    stop_input: Arc<AtomicBool>,
    prefix: PrefixKey,
    input_modes: Arc<std::sync::Mutex<term::InputModeState>>,
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
                // Only handle key-press events, to avoid duplicate input from key-up
                Event::Key(key_event)
                    if matches!(key_event.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                {
                    let modes = *input_modes.lock().unwrap();
                    for action in prefix_state.handle_key(key_event, prefix, modes) {
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
                Event::FocusGained => {
                    let _ = action_tx.send(AttachAction::Focus);
                }
                Event::Mouse(mouse_event) => {
                    let modes = *input_modes.lock().unwrap();
                    if let Some(bytes) = mouse_event_to_bytes(mouse_event, modes) {
                        let _ = action_tx.send(AttachAction::Input(bytes));
                    }
                }

                _ => {}
            }
        }
    })
}

async fn run_session_list_mode<W: Write>(
    current_session_id: &str,
    term_size: (u16, u16),
    stdout: &mut W,
) -> anyhow::Result<SessionListSelection> {
    let mut rows = build_session_list_rows(&list_sessions().await?, current_session_id);
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
            SessionListAction::Create => {
                // Inherit the cwd of the session under the cursor so the new session opens in the same directory;
                // if that session has no known cwd, fall back to create_session's default behavior (the client's current directory).
                let inherit_cwd = rows.get(selected).map(|row| row.cwd_raw.clone());
                match create_session_in(inherit_cwd.as_deref()).await {
                    Ok(new_session_id) => return Ok(SessionListSelection::Switch(new_session_id)),
                    Err(e) => {
                        status = format!("create failed: {e}");
                        render_session_list(stdout, &rows, selected, &status, term_size)?;
                    }
                }
            }
            SessionListAction::Rename => {
                if rows.is_empty() {
                    status = "no sessions".to_string();
                    render_session_list(stdout, &rows, selected, &status, term_size)?;
                    continue;
                }
                selected = selected.min(rows.len() - 1);
                let row = rows[selected].clone();
                if row.exited {
                    status = "cannot rename an exited session".to_string();
                    render_session_list(stdout, &rows, selected, &status, term_size)?;
                    continue;
                }
                match prompt_session_rename(stdout, &rows, selected, term_size, &row.name).await? {
                    Some(new_name) => match rename_session(&row.session_id, &new_name).await {
                        Ok(()) => {
                            rows = build_session_list_rows(
                                &list_sessions().await?,
                                current_session_id,
                            );
                            selected = rows
                                .iter()
                                .position(|r| r.session_id == row.session_id)
                                .unwrap_or_else(|| selected.min(rows.len().saturating_sub(1)));
                            status = format!("renamed to \"{new_name}\"");
                        }
                        Err(e) => status = format!("rename failed: {e}"),
                    },
                    None => status = String::new(),
                }
                render_session_list(stdout, &rows, selected, &status, term_size)?;
            }
            SessionListAction::Select => {
                if rows.is_empty() {
                    status = "no sessions".to_string();
                    render_session_list(stdout, &rows, selected, &status, term_size)?;
                    continue;
                }

                rows = build_session_list_rows(&list_sessions().await?, current_session_id);
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
                            KeyCode::Char('c') => return Ok(SessionListAction::Create),
                            KeyCode::Char('r') => return Ok(SessionListAction::Rename),
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

enum NameEditKey {
    Char(char),
    Backspace,
    Submit,
    Cancel,
}

async fn read_name_edit_key() -> anyhow::Result<NameEditKey> {
    tokio::task::spawn_blocking(|| {
        loop {
            match crossterm::event::poll(Duration::from_millis(50)) {
                Ok(true) => match crossterm::event::read() {
                    Ok(Event::Key(key_event))
                        if matches!(key_event.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                    {
                        match key_event.code {
                            KeyCode::Enter => return Ok(NameEditKey::Submit),
                            KeyCode::Esc => return Ok(NameEditKey::Cancel),
                            KeyCode::Backspace => return Ok(NameEditKey::Backspace),
                            KeyCode::Char(c) => return Ok(NameEditKey::Char(c)),
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

/// Prompt the user for a new name inline in the session list's bottom row. Returns Some(name) on submit, None on cancel.
async fn prompt_session_rename<W: Write>(
    stdout: &mut W,
    rows: &[SessionListRow],
    selected: usize,
    term_size: (u16, u16),
    old_name: &str,
) -> anyhow::Result<Option<String>> {
    let mut input = String::new();
    loop {
        let status = format!("rename \"{old_name}\" -> {input}_  (Enter confirm, Esc cancel)");
        render_session_list(stdout, rows, selected, &status, term_size)?;
        match read_name_edit_key().await? {
            NameEditKey::Submit => {
                let trimmed = input.trim();
                return Ok(if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_string())
                });
            }
            NameEditKey::Cancel => return Ok(None),
            NameEditKey::Backspace => {
                input.pop();
            }
            NameEditKey::Char(c) => {
                if !c.is_control() {
                    input.push(c);
                }
            }
        }
    }
}

/// Total visible width of all fixed columns before cwd in a session list row.
/// Layout: `{sel:1} {cur:1} {id:<4} {name:<24} {state:<14} {size:>8}  {cwd}`
/// = 1+1 +1+1 +4+1 +24+1 +14+1 +8+2 = 59.
const ROW_PREFIX_WIDTH: usize = 59;

fn render_session_list<W: Write>(
    out: &mut W,
    rows: &[SessionListRow],
    selected: usize,
    status: &str,
    term_size: (u16, u16),
) -> std::io::Result<()> {
    let (cols, rows_count) = term_size;
    write!(out, "\x1b[?2026h\x1b[0m\x1b[2J\x1b[H")?;
    write_text_line(out, "qscreen sessions", Some(color::sgr::HEADER), cols)?;
    write_session_list_hint(out, cols)?;
    write_text_line(out, "", None, cols)?;

    if rows.is_empty() {
        write_text_line(out, "  no sessions", Some(color::sgr::HINT), cols)?;
    } else {
        for (idx, row) in rows.iter().enumerate() {
            write_session_row(out, row, idx == selected, cols)?;
        }
    }

    let used_lines = rows.len() as u16 + 4;
    if rows_count > used_lines + 1 {
        write!(out, "\x1b[{};1H", rows_count)?;
    } else {
        write_text_line(out, "", None, cols)?;
    }
    let (status_text, status_sgr) = if status.is_empty() {
        ("* marks current session".to_string(), color::sgr::HINT)
    } else {
        (status.to_string(), status_style(status))
    };
    let status_text = truncate_for_terminal(&status_text, cols as usize);
    let visible = UnicodeWidthStr::width(status_text.as_str());
    // The status line is the last line and carries no CRLF (matching the original implementation).
    write!(out, "\x1b[0m")?;
    let colored = color::supported();
    if colored {
        write!(out, "\x1b[{status_sgr}m")?;
    }
    write!(out, "{status_text}")?;
    if colored {
        write!(out, "\x1b[0m")?;
    }
    for _ in visible.min(cols as usize)..cols as usize {
        out.write_all(b" ")?;
    }
    write!(out, "\x1b[?2026l")?;
    out.flush()
}

/// Pick the status line color by content: errors=red, successful rename=green, everything else=dim hint.
fn status_style(status: &str) -> &'static str {
    let lower = status.to_ascii_lowercase();
    if lower.contains("fail")
        || lower.contains("cannot")
        || lower.contains("error")
        || lower.contains("no sessions")
    {
        color::sgr::ERROR
    } else if lower.starts_with("renamed") {
        color::sgr::SUCCESS
    } else {
        color::sgr::HINT
    }
}

/// The session list's action hint line: highlight shortcut fragments (bold yellow) while keeping description text dim.
/// In a colorless environment it degrades to plain text with the same visible width, so the layout is unaffected.
fn write_session_list_hint<W: Write>(out: &mut W, cols: u16) -> std::io::Result<()> {
    // (fragment text, whether it is a shortcut key). Concatenated they form the original hint string, so it can be truncated by visible width.
    const SEGMENTS: &[(&str, bool)] = &[
        ("Up/Down", true),
        (" or ", false),
        ("k/j", true),
        (", ", false),
        ("Enter", true),
        (" switch, ", false),
        ("c", true),
        (" create, ", false),
        ("r", true),
        (" rename, ", false),
        ("Esc/q", true),
        (" cancel", false),
    ];
    let limit = cols as usize;
    let mut content = String::new();
    let mut visible = 0usize;
    for (text, is_key) in SEGMENTS {
        if visible >= limit {
            break;
        }
        let piece = truncate_for_terminal(text, limit - visible);
        if piece.is_empty() {
            continue;
        }
        visible += UnicodeWidthStr::width(piece.as_str());
        let sgr = if *is_key {
            color::sgr::KEY
        } else {
            color::sgr::HINT
        };
        // color::paint returns text unchanged in a colorless environment, so this branch covers both the colored and colorless cases.
        content.push_str(&color::paint(&piece, sgr));
    }
    write_list_line(out, &content, visible, None, cols)
}

/// Write a full line of text: truncate to `cols`, optionally color the whole line, pad the right with spaces, and end with CRLF.
fn write_text_line<W: Write>(
    out: &mut W,
    text: &str,
    sgr: Option<&str>,
    cols: u16,
) -> std::io::Result<()> {
    let truncated = truncate_for_terminal(text, cols as usize);
    let visible = UnicodeWidthStr::width(truncated.as_str());
    let content = match sgr {
        Some(s) => color::paint(&truncated, s),
        None => truncated,
    };
    write_list_line(out, &content, visible, None, cols)
}

/// Low-level line write: `content` is already truncated by visible width (may
/// contain ANSI codes), and `visible` is its visible width. `wrap_sgr` applies
/// a style across the whole line (including padding spaces), such as the reverse
/// video of the selected row; when `None`, the byte layout matches the original
/// `write_session_list_line` exactly, keeping existing tests stable.
fn write_list_line<W: Write>(
    out: &mut W,
    content: &str,
    visible: usize,
    wrap_sgr: Option<&str>,
    cols: u16,
) -> std::io::Result<()> {
    write!(out, "\x1b[0m")?;
    let wrap = wrap_sgr.filter(|_| color::supported());
    if let Some(s) = wrap {
        write!(out, "\x1b[{s}m")?;
    }
    out.write_all(content.as_bytes())?;
    for _ in visible.min(cols as usize)..cols as usize {
        out.write_all(b" ")?;
    }
    if wrap.is_some() {
        write!(out, "\x1b[0m")?;
    }
    write!(out, "\r\n")
}

fn write_session_row<W: Write>(
    out: &mut W,
    row: &SessionListRow,
    selected: bool,
    cols: u16,
) -> std::io::Result<()> {
    let selector = if selected { ">" } else { " " };

    // When the terminal is too narrow for the fixed columns, fall back to naive whole-line truncation (the selected row is still reverse-video).
    if (cols as usize) <= ROW_PREFIX_WIDTH {
        let current = if row.is_current { "*" } else { " " };
        let plain = format!(
            "{} {} {:<4} {:<24} {:<14} {:>8}  {}",
            selector,
            current,
            truncate_for_terminal(&row.session_id, 4),
            truncate_for_terminal(&row.name, 24),
            truncate_for_terminal(&row.state, 14),
            truncate_for_terminal(&row.size, 8),
            row.cwd
        );
        let truncated = truncate_for_terminal(&plain, cols as usize);
        let visible = UnicodeWidthStr::width(truncated.as_str());
        let wrap = if selected { Some("7") } else { None };
        return write_list_line(out, &truncated, visible, wrap, cols);
    }

    let cwd_budget = cols as usize - ROW_PREFIX_WIDTH;
    let id = truncate_for_terminal(&row.session_id, 4);
    let name = truncate_for_terminal(&row.name, 24);
    let state = truncate_for_terminal(&row.state, 14);
    let size = truncate_for_terminal(&row.size, 8);
    let cwd = truncate_for_terminal(&row.cwd, cwd_budget);
    let visible = ROW_PREFIX_WIDTH + UnicodeWidthStr::width(cwd.as_str());

    if selected {
        // Reverse-video the whole line; do not also apply per-column foreground colors, to avoid interfering with the reverse video.
        let current = if row.is_current { "*" } else { " " };
        let content = format!(
            "{} {} {:<4} {:<24} {:<14} {:>8}  {}",
            selector, current, id, name, state, size, cwd
        );
        write_list_line(out, &content, visible, Some("7"), cols)
    } else if color::supported() {
        // Per-column coloring: id cyan, name bold, state by status, size cyan, cwd blue, current marker green.
        let current = if row.is_current {
            color::paint("*", color::sgr::CURRENT)
        } else {
            " ".to_string()
        };
        let content = format!(
            "{} {} {} {} {} {}  {}",
            selector,
            current,
            color::paint(&format!("{id:<4}"), color::sgr::ID),
            color::paint(&format!("{name:<24}"), color::sgr::NAME),
            color::paint(
                &format!("{state:<14}"),
                color::state_sgr(row.exited, row.attached)
            ),
            color::paint(&format!("{size:>8}"), color::sgr::SIZE),
            color::paint(&cwd, color::sgr::CWD),
        );
        write_list_line(out, &content, visible, None, cols)
    } else {
        let current = if row.is_current { "*" } else { " " };
        let content = format!(
            "{} {} {:<4} {:<24} {:<14} {:>8}  {}",
            selector, current, id, name, state, size, cwd
        );
        write_list_line(out, &content, visible, None, cols)
    }
}

fn truncate_for_terminal(value: &str, width: usize) -> String {
    let mut visible = 0;
    value
        .chars()
        .take_while(|ch| {
            let next = visible + UnicodeWidthChar::width(*ch).unwrap_or(0);
            if next > width {
                return false;
            }
            visible = next;
            true
        })
        .collect()
}

fn is_prefix_key_event(event: crossterm::event::KeyEvent, prefix: PrefixKey) -> bool {
    if event.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(event.code, KeyCode::Char(c) if c.eq_ignore_ascii_case(&prefix.ctrl_char))
    {
        return true;
    }

    key_event_to_bytes(event, term::InputModeState::default()) == [prefix.byte]
}

fn key_char_eq_ignore_ascii_case(code: KeyCode, expected: char) -> bool {
    matches!(code, KeyCode::Char(c) if c.eq_ignore_ascii_case(&expected))
}

fn session_list_action_enabled() -> bool {
    true
}

// ── Connection helpers ───────────────────────────────────────────────────────

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

// ── Protocol helpers ─────────────────────────────────────────────────────────

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

async fn send_recv_ok(conn: &mut TcpConn, msg: Message) -> anyhow::Result<Message> {
    let id = msg.id.clone();
    send_msg(conn, msg).await?;
    let resp = recv_msg(conn).await?;
    check_response(&resp, &id)?;
    Ok(resp)
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

// ── Terminal size ────────────────────────────────────────────────────────────

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

    #[test]
    fn session_list_hint_renders_full_text_and_pads_to_width() {
        // In a colorless environment (the test default), the hint line should be plain text, right-padded with spaces to fill the whole line.
        let cols = 80u16;
        let mut out = Vec::new();
        write_session_list_hint(&mut out, cols).unwrap();
        let rendered = String::from_utf8(out).unwrap();
        assert!(rendered.ends_with("\r\n"));
        let line = rendered.strip_suffix("\r\n").unwrap();
        assert!(
            line.contains("Up/Down or k/j, Enter switch, c create, r rename, Esc/q cancel"),
            "{line:?}"
        );
        // After stripping any leading SGR reset prefix, the visible content should exactly fill cols columns.
        let visible = line.trim_start_matches("\x1b[0m");
        assert_eq!(UnicodeWidthStr::width(visible), cols as usize);
    }

    #[test]
    fn session_list_hint_truncates_to_narrow_width() {
        // On a very narrow terminal, truncate by visible width without panicking or exceeding the width.
        let cols = 5u16;
        let mut out = Vec::new();
        write_session_list_hint(&mut out, cols).unwrap();
        let rendered = String::from_utf8(out).unwrap();
        let line = rendered.strip_suffix("\r\n").unwrap();
        let visible = line.trim_start_matches("\x1b[0m");
        assert_eq!(UnicodeWidthStr::width(visible), cols as usize);
        assert!(visible.starts_with("Up/Do"), "{visible:?}");
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

    fn session(
        session_id: &str,
        name: &str,
        attached: bool,
        exited: bool,
        width: u32,
        height: u32,
    ) -> SessionInfo {
        SessionInfo {
            session_id: session_id.to_string(),
            name: name.to_string(),
            attached,
            exited,
            exit_code: if exited { 7 } else { 0 },
            created_at: chrono::DateTime::default(),
            width,
            height,
            size: String::new(),
            cwd: String::new(),
        }
    }

    #[test]
    fn parse_new_options_accepts_name() {
        let opts = parse_new_options(&[
            "--name".to_string(),
            "work".to_string(),
            "--shell=cmd".to_string(),
            "--cwd".to_string(),
            r"C:\work".to_string(),
        ])
        .unwrap();
        assert_eq!(opts.name.as_deref(), Some("work"));
        assert_eq!(opts.shell.as_deref(), Some("cmd"));
        assert_eq!(opts.cwd.as_deref(), Some(r"C:\work"));
    }

    #[test]
    fn parse_new_options_rejects_positional_name() {
        let err = parse_new_options(&["work".to_string()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("Use --name <name>"), "{err}");
    }

    #[test]
    fn cwd_for_request_defaults_to_client_directory() {
        let expected = std::env::current_dir().unwrap();
        let expected = cwd_request_from_path(&expected).unwrap();

        assert_eq!(cwd_for_request(None).unwrap(), expected);
        assert_eq!(cwd_for_request(Some("")).unwrap(), expected);
    }

    #[test]
    fn cwd_for_request_resolves_relative_path_from_client_directory() {
        let expected = std::env::current_dir().unwrap().join("project");
        let expected = cwd_request_from_path(&expected).unwrap();

        assert_eq!(cwd_for_request(Some("project")).unwrap(), expected);
    }

    #[test]
    fn cwd_for_request_preserves_absolute_path() {
        let path = if cfg!(windows) {
            r"C:\qscreen-project"
        } else {
            "/qscreen-project"
        };

        assert_eq!(
            cwd_for_request(Some(path)).unwrap(),
            CwdRequest {
                cwd: path.to_string(),
                cwd_bytes: Vec::new(),
            }
        );
    }

    #[test]
    fn cwd_ack_requires_matching_non_empty_response() {
        let requested = CwdRequest {
            cwd: "/work".to_string(),
            cwd_bytes: Vec::new(),
        };
        assert!(cwd_acknowledged(
            &requested,
            &Message {
                cwd: "/work".to_string(),
                ..Default::default()
            }
        ));
        assert!(!cwd_acknowledged(&requested, &Message::default()));
        assert!(!cwd_acknowledged(
            &requested,
            &Message {
                cwd: "/other".to_string(),
                ..Default::default()
            }
        ));
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_cwd_uses_raw_wire_bytes_and_requires_raw_ack() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let path = std::path::PathBuf::from(OsString::from_vec(b"/tmp/work-\xff".to_vec()));
        let requested = cwd_request_from_path(&path).unwrap();
        assert!(requested.cwd.is_empty());
        assert_eq!(requested.cwd_bytes, b"/tmp/work-\xff");
        assert!(cwd_acknowledged(
            &requested,
            &Message {
                cwd_bytes: requested.cwd_bytes.clone(),
                ..Default::default()
            }
        ));
        assert!(!cwd_acknowledged(&requested, &Message::default()));
    }

    #[test]
    fn focus_action_is_available_for_attach_loop() {
        assert_eq!(AttachAction::Focus, AttachAction::Focus);
    }

    #[tokio::test]
    async fn next_attach_message_prioritizes_pending_messages() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        tx.send(Message {
            kind: MessageKind::Event,
            event: Some(EventType::Frame),
            ..Default::default()
        })
        .unwrap();
        let mut pending = VecDeque::from([Message {
            kind: MessageKind::Response,
            id: "resize-1".to_string(),
            ..Default::default()
        }]);

        let msg = next_attach_message(&mut pending, &mut rx).await.unwrap();

        assert_eq!(msg.kind, MessageKind::Response);
        assert_eq!(msg.id, "resize-1");
        assert!(pending.is_empty());
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
    fn key_mapping_uses_application_cursor_and_modifiers() {
        assert_eq!(
            key_event_to_bytes(
                crossterm::event::KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
                term::InputModeState::default()
            ),
            b"\x1b[A".to_vec()
        );
        assert_eq!(
            key_event_to_bytes(
                crossterm::event::KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
                term::InputModeState {
                    application_cursor: true,
                    ..Default::default()
                }
            ),
            b"\x1bOA".to_vec()
        );
        assert_eq!(
            key_event_to_bytes(
                crossterm::event::KeyEvent::new(
                    KeyCode::Right,
                    KeyModifiers::SHIFT | KeyModifiers::CONTROL
                ),
                term::InputModeState::default()
            ),
            b"\x1b[1;6C".to_vec()
        );
        assert_eq!(
            key_event_to_bytes(
                crossterm::event::KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT),
                term::InputModeState::default()
            ),
            b"\x1b[Z".to_vec()
        );
    }

    #[test]
    fn mouse_mapping_uses_sgr_mode() {
        let modes = term::InputModeState {
            mouse_mode: FrameMouseMode::PressRelease,
            mouse_encoding: FrameMouseEncoding::Sgr,
            ..Default::default()
        };
        let down = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 4,
            row: 9,
            modifiers: KeyModifiers::CONTROL,
        };
        let up = MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 4,
            row: 9,
            modifiers: KeyModifiers::NONE,
        };

        assert_eq!(
            mouse_event_to_bytes(down, modes).unwrap(),
            b"\x1b[<16;5;10M".to_vec()
        );
        assert_eq!(
            mouse_event_to_bytes(up, modes).unwrap(),
            b"\x1b[<3;5;10m".to_vec()
        );
        assert!(mouse_event_to_bytes(down, term::InputModeState::default()).is_none());
    }

    #[test]
    fn mouse_mapping_uses_utf8_mode_for_large_coordinates() {
        let modes = term::InputModeState {
            mouse_mode: FrameMouseMode::PressRelease,
            mouse_encoding: FrameMouseEncoding::Utf8,
            ..Default::default()
        };
        let event = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 300,
            row: 1,
            modifiers: KeyModifiers::NONE,
        };

        let mut expected = b"\x1b[M ".to_vec();
        expected.extend_from_slice("ō".as_bytes());
        expected.push(b'"');
        assert_eq!(mouse_event_to_bytes(event, modes).unwrap(), expected);
    }

    #[test]
    fn prefix_state_detaches_for_default_and_custom_prefixes() {
        for prefix in [DEFAULT_PREFIX, PrefixKey::parse("C-b").unwrap()] {
            let mut state = PrefixState::default();
            assert_eq!(
                state.handle_key(
                    ctrl_key(prefix.ctrl_char),
                    prefix,
                    term::InputModeState::default()
                ),
                vec![]
            );
            assert_eq!(
                state.handle_key(char_key('d'), prefix, term::InputModeState::default()),
                vec![AttachAction::Detach]
            );
        }
    }

    #[test]
    fn prefix_state_accepts_uppercase_actions() {
        let mut state = PrefixState::default();
        assert_eq!(
            state.handle_key(
                ctrl_key('a'),
                DEFAULT_PREFIX,
                term::InputModeState::default()
            ),
            vec![]
        );
        assert_eq!(
            state.handle_key(
                char_key('D'),
                DEFAULT_PREFIX,
                term::InputModeState::default()
            ),
            vec![AttachAction::Detach]
        );

        let mut state = PrefixState::default();
        assert_eq!(
            state.handle_key(
                ctrl_key('a'),
                DEFAULT_PREFIX,
                term::InputModeState::default()
            ),
            vec![]
        );
        assert_eq!(
            state.handle_key(
                char_key('S'),
                DEFAULT_PREFIX,
                term::InputModeState::default()
            ),
            vec![AttachAction::OpenSessionList]
        );
    }

    #[test]
    fn prefix_state_sends_literal_prefix_for_double_prefix() {
        for prefix in [DEFAULT_PREFIX, PrefixKey::parse("C-b").unwrap()] {
            let mut state = PrefixState::default();
            assert_eq!(
                state.handle_key(
                    ctrl_key(prefix.ctrl_char),
                    prefix,
                    term::InputModeState::default()
                ),
                vec![]
            );
            assert_eq!(
                state.handle_key(
                    ctrl_key(prefix.ctrl_char),
                    prefix,
                    term::InputModeState::default()
                ),
                vec![AttachAction::Input(vec![prefix.byte])]
            );
        }
    }

    #[test]
    fn prefix_state_falls_back_to_literal_prefix_then_normal_byte() {
        for prefix in [DEFAULT_PREFIX, PrefixKey::parse("C-b").unwrap()] {
            let mut state = PrefixState::default();
            assert_eq!(
                state.handle_key(
                    ctrl_key(prefix.ctrl_char),
                    prefix,
                    term::InputModeState::default()
                ),
                vec![]
            );
            assert_eq!(
                state.handle_key(char_key('x'), prefix, term::InputModeState::default()),
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
            state.handle_key(
                char_key('d'),
                DEFAULT_PREFIX,
                term::InputModeState::default()
            ),
            vec![AttachAction::Input(vec![b'd'])]
        );
    }

    #[test]
    fn prefix_state_opens_session_list_for_default_and_custom_prefixes() {
        for prefix in [DEFAULT_PREFIX, PrefixKey::parse("C-b").unwrap()] {
            let mut state = PrefixState::default();
            assert_eq!(
                state.handle_key(
                    ctrl_key(prefix.ctrl_char),
                    prefix,
                    term::InputModeState::default()
                ),
                vec![]
            );
            assert_eq!(
                state.handle_key(char_key('s'), prefix, term::InputModeState::default()),
                vec![AttachAction::OpenSessionList]
            );
        }
    }

    #[test]
    fn prefix_state_uses_raw_custom_prefix_bytes() {
        let prefix = PrefixKey::parse("C-b").unwrap();

        let mut state = PrefixState::default();
        assert_eq!(
            state.handle_key(raw_char_key(0x02), prefix, term::InputModeState::default()),
            vec![]
        );
        assert_eq!(
            state.handle_key(char_key('d'), prefix, term::InputModeState::default()),
            vec![AttachAction::Detach]
        );

        let mut state = PrefixState::default();
        assert_eq!(
            state.handle_key(raw_char_key(0x02), prefix, term::InputModeState::default()),
            vec![]
        );
        assert_eq!(
            state.handle_key(char_key('s'), prefix, term::InputModeState::default()),
            vec![AttachAction::OpenSessionList]
        );
    }

    #[test]
    fn session_list_rows_sort_like_list_output_and_mark_current() {
        let sessions = vec![
            session("3", "zeta", false, false, 100, 40),
            session("1", "alpha", true, false, 80, 24),
            session("2", "mid", false, true, 120, 30),
        ];
        let rows = build_session_list_rows(&sessions, "1");

        assert_eq!(
            rows.iter()
                .map(|row| row.session_id.as_str())
                .collect::<Vec<_>>(),
            vec!["1", "2", "3"]
        );
        assert!(rows[0].is_current);
        assert_eq!(rows[0].name, "alpha");
        assert_eq!(rows[0].state, "attached");
        assert_eq!(rows[0].size, "80x24");
        assert_eq!(rows[1].state, "exited(7)");
        assert_eq!(rows[2].state, "detached");
    }

    #[test]
    fn session_list_rows_prefer_protocol_size_label() {
        let mut info = session("1", "work", false, false, 80, 24);
        info.size = "132x43".to_string();
        let rows = build_session_list_rows(&[info], "2");

        assert_eq!(rows[0].size, "132x43");
    }

    #[test]
    fn selection_rules_cover_current_exited_attached_and_attachable() {
        let rows = build_session_list_rows(
            &[
                session("1", "current", true, false, 80, 24),
                session("2", "done", false, true, 80, 24),
                session("3", "busy", true, false, 80, 24),
                session("4", "next", false, false, 80, 24),
            ],
            "1",
        );

        assert_eq!(
            selection_for_session_row(rows.iter().find(|row| row.name == "current").unwrap()),
            SessionListSelection::Close
        );
        assert_eq!(
            selection_for_session_row(rows.iter().find(|row| row.name == "done").unwrap()),
            SessionListSelection::Error(exited_session_error("2"))
        );
        assert_eq!(
            selection_for_session_row(rows.iter().find(|row| row.name == "busy").unwrap()),
            SessionListSelection::Switch("3".to_string())
        );
        assert_eq!(
            selection_for_session_row(rows.iter().find(|row| row.name == "next").unwrap()),
            SessionListSelection::Switch("4".to_string())
        );
    }

    #[test]
    fn format_session_line_uses_attached_bool_for_live_sessions() {
        assert_eq!(
            format_session_line(&session("1", "work", true, false, 100, 30)),
            "1\twork\tattached\t-\t100x30\t-"
        );
        assert_eq!(
            format_session_line(&session("2", "idle", false, false, 80, 24)),
            "2\tidle\tdetached\t-\t80x24\t-"
        );
    }

    #[test]
    fn format_session_line_includes_cwd_when_present() {
        let mut info = session("1", "work", false, false, 80, 24);
        info.cwd = r"C:\work".to_string();
        assert_eq!(
            format_session_line(&info),
            "1\twork\tdetached\t-\t80x24\tC:\\work"
        );
    }

    #[test]
    fn session_cwd_label_escapes_terminal_control_characters() {
        let mut info = session("1", "work", false, false, 80, 24);
        info.cwd = "/work\n\t\u{1b}\u{85}\u{7f}".to_string();

        let label = session_cwd_label(&info);

        assert_eq!(label, r"/work\n\t\u{1b}\u{85}\u{7f}");
        assert!(!label.chars().any(char::is_control));
    }

    #[test]
    fn terminal_truncation_uses_display_width() {
        assert_eq!(truncate_for_terminal("ab界c", 3), "ab");
        assert_eq!(truncate_for_terminal("ab界c", 4), "ab界");
        assert_eq!(truncate_for_terminal("e\u{301}x", 1), "e\u{301}");
    }

    #[test]
    fn text_line_padding_uses_display_width() {
        let mut out = Vec::new();

        write_text_line(&mut out, "界", None, 4).unwrap();

        assert_eq!(String::from_utf8(out).unwrap(), "\x1b[0m界  \r\n");
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
                session("1", "main", true, false, 80, 24),
                session("2", "work", false, false, 100, 30),
            ],
            "1",
        );
        let mut out = Vec::new();

        render_session_list(&mut out, &rows, 1, "ready", (80, 24)).unwrap();

        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("qscreen sessions"));
        assert!(text.contains("  * 1    main"));
        assert!(text.contains(">   2    work"));
        assert!(text.contains("ready"));
    }

    #[test]
    fn render_session_list_uses_crlf_in_raw_mode() {
        let rows = build_session_list_rows(&[session("1", "main", true, false, 80, 24)], "1");
        let mut out = Vec::new();

        render_session_list(&mut out, &rows, 0, "", (80, 24)).unwrap();

        for (idx, byte) in out.iter().enumerate() {
            if *byte == b'\n' {
                assert_eq!(idx.checked_sub(1).map(|prev| out[prev]), Some(b'\r'));
            }
        }
        assert!(out.windows(2).any(|window| window == b"\r\n"));
    }

    #[test]
    fn render_session_list_resets_style_and_pads_lines() {
        let rows = build_session_list_rows(&[session("1", "main", true, false, 80, 24)], "1");
        let mut out = Vec::new();

        render_session_list(&mut out, &rows, 0, "", (20, 8)).unwrap();

        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("\x1b[?2026h\x1b[0m\x1b[2J"));
        assert!(text.contains("\x1b[0mqscreen sessions    \r\n"));
        assert!(text.contains("\x1b[0m> * 1    main"));
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

    #[test]
    fn default_attach_request_remains_frame_omitted() {
        let msg = attach_request_message("1", "42", 80, 24);

        assert_eq!(msg.attach_mode, qscreen_protocol::AttachMode::Frame);
        let line = msg.to_json_line().unwrap();
        let json = std::str::from_utf8(&line).unwrap();
        assert!(!json.contains("attach_mode"));
    }
}
