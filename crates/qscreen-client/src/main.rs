use std::collections::VecDeque;
use std::io::Write;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::time::{Duration, Instant};

use anyhow::Context;
use crossterm::event::{
    Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use qscreen_protocol::{
    Command, EventType, FrameMouseEncoding, FrameMouseMode, Message, MessageKind, ScreenFrame,
    SessionInfo, exited_session_error, missing_session_error, validate_session_id,
    validate_session_name,
};
use qscreen_shared::{daemon_log_path, pipe_name};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

mod color;
mod term;

const DEFAULT_PREFIX: PrefixKey = PrefixKey {
    ctrl_char: 'B',
    byte: 0x02,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PrefixKey {
    ctrl_char: char,
    byte: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ClientConfig {
    prefix: PrefixKey,
    status_bar: bool,
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
    parse_client_config_with_env(
        args,
        std::env::var("QSCREEN_PREFIX").ok(),
        std::env::var("QSCREEN_STATUS_BAR").ok(),
    )
}

fn parse_client_config_with_env(
    args: Vec<String>,
    env_prefix: Option<String>,
    env_status_bar: Option<String>,
) -> anyhow::Result<(ClientConfig, Vec<String>)> {
    let (options, remaining_args) = take_client_options(args)?;
    let prefix = match options.prefix.or(env_prefix) {
        Some(value) => PrefixKey::parse(&value)?,
        None => DEFAULT_PREFIX,
    };
    let status_bar = match options.status_bar.or(env_status_bar) {
        Some(value) => parse_status_bar_value(&value)?,
        None => true,
    };
    Ok((ClientConfig { prefix, status_bar }, remaining_args))
}

#[derive(Debug, Default)]
struct ClientOptionArgs {
    prefix: Option<String>,
    status_bar: Option<String>,
}

fn take_client_options(args: Vec<String>) -> anyhow::Result<(ClientOptionArgs, Vec<String>)> {
    let mut options = ClientOptionArgs::default();
    let mut remaining = Vec::with_capacity(args.len());
    let mut iter = args.into_iter();

    while let Some(arg) = iter.next() {
        if arg == "--prefix" {
            let Some(value) = iter.next() else {
                anyhow::bail!("invalid prefix: --prefix requires a value");
            };
            options.prefix = Some(value);
        } else if let Some(value) = arg.strip_prefix("--prefix=") {
            options.prefix = Some(value.to_string());
        } else if arg == "--status-bar" {
            let Some(value) = iter.next() else {
                anyhow::bail!("invalid status bar option: --status-bar requires on or off");
            };
            options.status_bar = Some(value);
        } else if let Some(value) = arg.strip_prefix("--status-bar=") {
            options.status_bar = Some(value.to_string());
        } else {
            remaining.push(arg);
        }
    }

    Ok((options, remaining))
}

fn parse_status_bar_value(value: &str) -> anyhow::Result<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "on" | "1" | "true" | "yes" => Ok(true),
        "off" | "0" | "false" | "no" => Ok(false),
        _ => anyhow::bail!("invalid status bar option `{value}`: expected on or off"),
    }
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
                print_help(config.prefix);
                Ok(())
            }
            [cmd] if cmd == "-V" || cmd == "--version" => {
                print_version();
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
            [cmd] if cmd == "attach" || cmd == "att" || cmd == "-r" => {
                cmd_attach_default(config).await
            }
            [cmd, session_id] if cmd == "attach" || cmd == "att" || cmd == "-r" => {
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

fn print_version() {
    println!("qscn {}", env!("CARGO_PKG_VERSION"));
}

/// One in-session prefix key binding. `keys` is the key(s) pressed after the
/// prefix; the literal `<prefix>` token is expanded to the active prefix label.
struct PrefixBinding {
    keys: &'static str,
    desc_en: &'static str,
    desc_zh: &'static str,
}

/// Single source of truth for the `<prefix>`-driven in-session commands, shared
/// by the CLI `--help` output and the in-session `<prefix> ?` help screen.
const PREFIX_BINDINGS: &[PrefixBinding] = &[
    PrefixBinding {
        keys: "?",
        desc_en: "show this key-binding help (Esc or q to close)",
        desc_zh: "显示此快捷键帮助(按 Esc 或 q 关闭)",
    },
    PrefixBinding {
        keys: "d",
        desc_en: "detach from the session (it keeps running in the background)",
        desc_zh: "从当前会话 detach(会话继续在后台运行)",
    },
    PrefixBinding {
        keys: "s",
        desc_en: "open the session list (Enter switch, c create, r rename, x kill, q cancel)",
        desc_zh: "打开会话列表(Enter 切换,c 新建,r 改名,x 终止,q 取消)",
    },
    PrefixBinding {
        keys: "n",
        desc_en: "switch to the next session (by ID, wraps around)",
        desc_zh: "切换到下一个会话(按 ID 顺序,末尾回到开头)",
    },
    PrefixBinding {
        keys: "p",
        desc_en: "switch to the previous session (by ID, wraps around)",
        desc_zh: "切换到上一个会话(按 ID 顺序,开头回到末尾)",
    },
    PrefixBinding {
        keys: "<prefix>",
        desc_en: "send a literal prefix key to the terminal",
        desc_zh: "向终端发送字面前缀字符",
    },
];

/// Human-readable label for a prefix key, e.g. `Ctrl+B`.
fn prefix_label(prefix: PrefixKey) -> String {
    format!("Ctrl+{}", prefix.ctrl_char)
}

/// Width of the indented `Ctrl+X key` column before the description text, so the
/// descriptions line up across bindings. Combos are ASCII, so character count
/// equals display width here.
const PREFIX_COMBO_WIDTH: usize = 28;

/// Build the aligned `<prefix>`-binding rows for the given prefix, localized by
/// `zh`. Each row is `(combo_field, description)`: `combo_field` is the indented,
/// width-padded `Ctrl+X key` column meant to be rendered in the KEY color and the
/// description in the HINT color. Shared by both `--help` and the in-session help
/// screen so they never drift and highlight the keys identically.
fn prefix_help_rows(prefix: PrefixKey, zh: bool) -> Vec<(String, String)> {
    let label = prefix_label(prefix);
    PREFIX_BINDINGS
        .iter()
        .map(|b| {
            let keys = b.keys.replace("<prefix>", &label);
            let combo = format!("{label} {keys}");
            let desc = if zh { b.desc_zh } else { b.desc_en };
            (
                format!("  {combo:<width$}", width = PREFIX_COMBO_WIDTH),
                desc.to_string(),
            )
        })
        .collect()
}

/// The `--help` key-binding block: combos highlighted in KEY (bold yellow),
/// descriptions in HINT (dim). Degrades to plain aligned text without color.
fn prefix_help_block(prefix: PrefixKey, zh: bool) -> String {
    prefix_help_rows(prefix, zh)
        .iter()
        .map(|(combo, desc)| {
            format!(
                "{}{}",
                color::paint(combo, color::sgr::KEY),
                color::paint(desc, color::sgr::HINT)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn print_help(prefix: PrefixKey) {
    let zh = is_chinese();
    let raw = if zh { help_text_zh() } else { help_text_en() };
    let raw = raw.replace("%PREFIX_BINDINGS%", &prefix_help_block(prefix, zh));
    print!("{}", colorize_help(&raw));
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
  qscn [--prefix C-b]          智能启动：无会话时新建并进入，单会话时直接 attach，
                            多会话时列出所有会话
  qscn [--prefix C-b] new
                               新建自动命名会话并进入
  qscn [--prefix C-b] new --name <name>
                               用参数指定显示名
  qscn [--prefix C-b] new --shell <shell>
                               指定启动 shell（Windows: cmd、powershell 或可执行文件路径；Unix: shell 路径）
  qscn [--prefix C-b] new --cwd <path>
                               指定会话启动工作目录
  qscn [--prefix C-b] attach [session_id]
                               进入已有会话；省略 session_id 时进入 ID 最大的可用会话（别名 att）
  qscn [--prefix C-b] -r [session_id]
                               同 attach，兼容 tmux 风格
  qscn ls                      列出所有会话（同 list）
  qscn list                    列出所有会话
  qscn kill <session_id>       强制终止指定会话
  qscn rename <session_id> <name>
                               修改会话显示名
  qscn shutdown                停止后台 daemon（所有会话将被关闭）
  qscn -h, --help              显示此帮助
  qscn -V, --version           显示版本号

前缀:
  默认前缀为 Ctrl+B（C-b）
  --prefix C-a                 使用 Ctrl+A 作为当前命令的会话前缀
  QSCREEN_PREFIX=C-a           为所有命令设置备用前缀
  支持 C-a..C-z 或 Ctrl+A..Ctrl+Z；CLI 参数优先于环境变量

状态栏:
  attach 时底部一行列出所有会话（* 当前，! 已退出），每 2 秒刷新
  --status-bar off             本次命令关闭状态栏
  QSCREEN_STATUS_BAR=off       为所有命令关闭状态栏
  取值 on|off；CLI 参数优先于环境变量；终端高度不足 3 行时自动停用

会话内热键（默认前缀 Ctrl+B，按下前缀后再按对应键）:
%PREFIX_BINDINGS%

ls 输出格式:
  <session_id>  <name>  <状态>  <创建时间>  <终端尺寸>
  状态: attached | detached | exited(<退出码>)

示例:
  qscn                         # 自动进入唯一会话，或新建自动命名会话
  qscn new                     # 新建自动分配 session_id 的会话
  qscn new --name work         # 新建显示名为 work 的会话
  qscn new --shell cmd --name work
  qscn new --cwd C:\work --name work
  qscn --prefix C-a attach 1   # 使用 Ctrl+A 作为前缀进入 session_id=1
  qscn attach 1                # 重新进入 session_id=1
  qscn rename 1 work           # 修改 session_id=1 的显示名
  qscn ls                      # 查看所有会话状态
  qscn kill 1                  # 终止 session_id=1
"#
}

fn help_text_en() -> &'static str {
    r#"qscreen — lightweight terminal session manager

Usage:
  qscn [--prefix C-b]          smart launch: create and enter a session if no sessions,
                            attach if one session, list all if multiple
  qscn [--prefix C-b] new
                               create an auto-named session and attach
  qscn [--prefix C-b] new --name <name>
                               specify the display name as an option
  qscn [--prefix C-b] new --shell <shell>
                               specify the startup shell (Windows: cmd, powershell, or an executable path; Unix: shell path)
  qscn [--prefix C-b] new --cwd <path>
                               specify the session working directory
  qscn [--prefix C-b] attach [session_id]
                               attach to an existing session; without session_id, attach to the highest-ID available session (alias: att)
  qscn [--prefix C-b] -r [session_id]
                               same as attach (tmux-style shorthand)
  qscn ls                      list all sessions (alias: list)
  qscn list                    list all sessions
  qscn kill <session_id>       forcibly terminate a session
  qscn rename <session_id> <name>
                               change a session display name
  qscn shutdown                stop the background daemon (closes all sessions)
  qscn -h, --help              show this help
  qscn -V, --version           show the version

Prefix:
  the default prefix is Ctrl+B (C-b)
  --prefix C-a                 use Ctrl+A as the session prefix for this command
  QSCREEN_PREFIX=C-a           set a fallback prefix for every command
  Values: C-a..C-z or Ctrl+A..Ctrl+Z; CLI takes precedence over env

Status bar:
  while attached, the bottom row lists live sessions (* marks current), refreshed every 2s
  --status-bar off             disable the status bar for this command
  QSCREEN_STATUS_BAR=off       disable the status bar for every command
  Values: on|off; CLI takes precedence over env; disabled when the terminal has fewer than 3 rows

Key bindings (default prefix Ctrl+B; press the prefix, then the key):
%PREFIX_BINDINGS%

ls output format:
  <session_id>  <name>  <state>  <created-at>  <terminal-size>
  states: attached | detached | exited(<code>)

Examples:
  qscn                         # auto-attach or create an auto-named session
  qscn new                     # create a session with an auto-assigned session_id
  qscn new --name work         # create a session with display name 'work'
  qscn new --shell cmd --name work
  qscn new --cwd C:\work --name work
  qscn --prefix C-a attach 1   # attach to session_id=1 using Ctrl+A as the prefix
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
    // Only the interactive `qscn new` path surfaces the Claude Code flicker hint,
    // and only on this first attach (reattaches inside the loop pass None).
    let notice = claude_no_flicker_notice();
    attach_session_loop(&session_id, config, notice).await
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
    attach_session_loop(session_id, config, None).await
}

/// `qscn attach` without an id: attach to the highest-ID live session, or
/// error when no attachable session exists. If the chosen session exits in the
/// window between listing and the attach handshake, fall back to the next live
/// session instead of failing outright.
async fn cmd_attach_default(config: ClientConfig) -> anyhow::Result<()> {
    term::preflight_interactive()?;
    // Sessions that raced out (exited between selection and attach) and must be
    // skipped on the next selection so we do not retry a dead candidate.
    let mut skip: Vec<String> = Vec::new();
    loop {
        let sessions = list_sessions().await?;
        let Some(session_id) = highest_live_session_id_excluding(&sessions, &skip) else {
            if is_chinese() {
                anyhow::bail!("没有可用的 session")
            } else {
                anyhow::bail!("no attachable session")
            }
        };
        match attach_session_loop(&session_id, config, None).await {
            Ok(()) => return Ok(()),
            // The chosen session exited between listing and the attach handshake:
            // skip it and pick the next live session. Any other error (connection
            // loss, daemon crash, a failure after a successful attach) is surfaced
            // unchanged rather than being misread as a race.
            Err(err) if err.downcast_ref::<SessionUnavailable>().is_some() => {
                skip.push(session_id);
                continue;
            }
            Err(err) => return Err(err),
        }
    }
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
        // The `qscn list` textual output stays English so it remains a stable,
        // script-parseable format regardless of locale.
        session_state_label(s, false),
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
            &session_state_label(s, false),
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
    /// Switch to another session after the current, attached session was killed
    /// from the list. The dead session must not be kept as a reattach fallback.
    SwitchFresh(String),
    /// The last session was killed from the list, so qscn should shut the daemon
    /// down and exit.
    Quit,
}

fn build_session_list_rows(
    sessions: &[SessionInfo],
    current_session_id: &str,
    zh: bool,
) -> Vec<SessionListRow> {
    let mut rows: Vec<SessionListRow> = sessions
        .iter()
        .map(|session| SessionListRow {
            session_id: session.session_id.clone(),
            name: session.name.clone(),
            state: session_state_label(session, zh),
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

fn session_state_label(session: &SessionInfo, zh: bool) -> String {
    if session.exited {
        if zh {
            format!("已退出({})", session.exit_code)
        } else {
            format!("exited({})", session.exit_code)
        }
    } else if session.attached {
        if zh { "已连接" } else { "attached" }.to_string()
    } else if zh {
        "未连接".to_string()
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

// ── Status bar ───────────────────────────────────────────────────────────────

/// Below this terminal height the status bar is disabled and attach uses the
/// full height, so tiny terminals keep every row for the session.
const STATUS_BAR_MIN_HEIGHT: u16 = 3;
/// How often the attach loop polls the daemon for the session list shown in
/// the status bar.
const STATUS_BAR_POLL_INTERVAL: Duration = Duration::from_secs(2);
/// Give up on a status bar poll quickly so a stalled daemon cannot wedge the
/// attach loop; the side connection is dropped and reopened on the next tick.
const STATUS_BAR_FETCH_TIMEOUT: Duration = Duration::from_millis(800);
/// The whole status bar row is reverse video, so it reads as a solid,
/// theme-adaptive strip: on a dark theme it becomes a light bar, on a light
/// theme a dark bar, either way contrasting with the terminal content above it.
/// Because reverse swaps foreground and background, the current session is
/// conveyed by a plaintext `*` marker plus a subdued attribute rather than a
/// foreground color, which would otherwise turn into a background tint. Exited
/// sessions are not listed at all.
const STATUS_BAR_BASE_SGR: &str = "7";
/// Header `[qscn]`: reverse video plus bold.
const STATUS_BAR_HEADER_SGR: &str = "7;1";
/// Current (attached) session: normal video plus bold, so it reads as a bright
/// cutout standing out from the inverted bar around it. Paired with a leading
/// `*` marker on the left of the session id.
const STATUS_BAR_CURRENT_SGR: &str = "1";

fn status_bar_active(config: ClientConfig, term_height: u16) -> bool {
    config.status_bar && term_height >= STATUS_BAR_MIN_HEIGHT
}

/// Height reported to the daemon: when the status bar is active, the bottom
/// row is reserved for it and the session gets the rest.
fn session_area_height(config: ClientConfig, term_height: u16) -> u16 {
    if status_bar_active(config, term_height) {
        term_height - 1
    } else {
        term_height
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StatusBarItem {
    session_id: String,
    name: String,
    is_current: bool,
}

/// Build the status bar's session list. Exited sessions are omitted: the bar
/// only shows live sessions the user can switch to.
fn build_status_bar_items(
    sessions: &[SessionInfo],
    current_session_id: &str,
) -> Vec<StatusBarItem> {
    let mut items: Vec<StatusBarItem> = sessions
        .iter()
        .filter(|session| !session.exited)
        .map(|session| StatusBarItem {
            session_id: session.session_id.clone(),
            name: session.name.clone(),
            is_current: session.session_id == current_session_id,
        })
        .collect();
    items.sort_by_key(|item| item.session_id.parse::<u64>().unwrap_or(u64::MAX));
    items
}

/// One session's `(text, sgr)` segment. The current session carries its `*`
/// marker on the left of the id (`*3:work`); the marker doubles as a colorless
/// fallback for the styling.
fn status_bar_item_segment(item: &StatusBarItem) -> (String, &'static str) {
    if item.is_current {
        (
            format!("*{}:{}", item.session_id, item.name),
            STATUS_BAR_CURRENT_SGR,
        )
    } else {
        (
            format!("{}:{}", item.session_id, item.name),
            STATUS_BAR_BASE_SGR,
        )
    }
}

/// The pinned `[qscn]` header plus the scrollable items region. `segs`
/// interleaves a single-space separator (reverse video, so the bar stays
/// continuous) before each session segment. `total` is the items region's full
/// width in columns; `current_span`, when present, is the `[start, end)` column
/// range of the current session's text within that region (separator excluded),
/// used to keep that session fully visible while scrolling.
struct StatusBarLayout {
    header: (String, &'static str),
    segs: Vec<(String, &'static str)>,
    total: usize,
    current_span: Option<(usize, usize)>,
}

fn build_status_bar_layout(items: &[StatusBarItem]) -> StatusBarLayout {
    let mut segs: Vec<(String, &'static str)> = Vec::with_capacity(items.len() * 2);
    let mut col = 0usize;
    let mut current_span = None;
    for item in items {
        segs.push((" ".to_string(), STATUS_BAR_BASE_SGR));
        col += 1;
        let (text, sgr) = status_bar_item_segment(item);
        let width = UnicodeWidthStr::width(text.as_str());
        if item.is_current {
            current_span = Some((col, col + width));
        }
        col += width;
        segs.push((text, sgr));
    }
    StatusBarLayout {
        header: ("[qscn]".to_string(), STATUS_BAR_HEADER_SGR),
        segs,
        total: col,
        current_span,
    }
}

/// Flattened header + items segments, whose concatenated text is the full bar
/// content. Rendering uses [`build_status_bar_layout`] directly so it can pin
/// the header and scroll the items region; this helper exists for tests that
/// reason about the plaintext form.
#[cfg(test)]
fn status_bar_segments(items: &[StatusBarItem]) -> Vec<(String, &'static str)> {
    let layout = build_status_bar_layout(items);
    let mut segments = vec![layout.header];
    segments.extend(layout.segs);
    segments
}

/// Horizontal scroll offset (in columns) into the items region so the current
/// session stays fully visible. Zero when everything fits. When the current
/// session is itself wider than the region, aligns to its start so at least the
/// id and the beginning of the name show.
fn status_bar_scroll_offset(
    total: usize,
    region: usize,
    current_span: Option<(usize, usize)>,
) -> usize {
    if region == 0 || total <= region {
        return 0;
    }
    let Some((start, end)) = current_span else {
        return 0;
    };
    if end - start >= region {
        return start;
    }
    let mut offset = 0;
    if end > region {
        offset = end - region;
    }
    if start < offset {
        offset = start;
    }
    offset.min(total - region)
}

/// Slice `text` to the column window `[skip, skip + take)`, dropping any wide
/// grapheme that would straddle either edge. Session ids and names are ASCII, so
/// in practice every cell is exactly one column wide.
fn slice_columns(text: &str, skip: usize, take: usize) -> String {
    let mut col = 0usize;
    let mut out = String::new();
    for ch in text.chars() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if col + w <= skip {
            col += w;
            continue;
        }
        if col < skip {
            // Wide char straddling the left edge: drop it rather than render a half.
            col += w;
            continue;
        }
        if (col - skip) + w > take {
            break;
        }
        out.push(ch);
        col += w;
    }
    out
}

/// Draw the status bar when active; a no-op while the item cache is still
/// empty (before the first poll completes) so no stale row is painted over.
fn draw_status_bar<W: Write>(
    out: &mut W,
    items: &[StatusBarItem],
    config: ClientConfig,
    term_size: (u16, u16),
    cursor_visible: bool,
) -> std::io::Result<()> {
    if !status_bar_active(config, term_size.1) || items.is_empty() {
        return Ok(());
    }
    render_status_bar(out, items, term_size, cursor_visible)
}

/// Render the bar on the terminal's bottom row. The cursor is saved/restored
/// (DECSC/DECRC) around the write and hidden while drawing, so the session
/// area and the frame-rendered cursor position are unaffected.
fn render_status_bar<W: Write>(
    out: &mut W,
    items: &[StatusBarItem],
    term_size: (u16, u16),
    cursor_visible: bool,
) -> std::io::Result<()> {
    let (cols, rows) = term_size;
    let cols = cols as usize;
    write!(out, "\x1b[?2026h\x1b7\x1b[?25l\x1b[0m\x1b[{};1H", rows)?;

    let layout = build_status_bar_layout(items);
    let mut written = 0usize;

    // Pinned header, truncated only on a very narrow terminal.
    written += emit_bar_segment(out, &layout.header.0, layout.header.1, cols)?;

    // Scrollable items region: scroll so the current session stays fully visible.
    let region = cols - written; // cols >= written always
    let offset = status_bar_scroll_offset(layout.total, region, layout.current_span);
    let mut pos = 0usize; // column position within the items region
    for (text, sgr) in &layout.segs {
        if written >= cols {
            break;
        }
        let width = UnicodeWidthStr::width(text.as_str());
        let seg_end = pos + width;
        // Skip segments scrolled entirely off the left edge.
        if seg_end <= offset {
            pos = seg_end;
            continue;
        }
        let left_skip = offset.saturating_sub(pos);
        let take = (cols - written).min(width - left_skip);
        let piece = slice_columns(text, left_skip, take);
        written += emit_bar_segment(out, &piece, sgr, cols - written)?;
        pos = seg_end;
    }

    // Fill the rest of the row so the reversed bar spans the full width.
    if written < cols {
        let pad = " ".repeat(cols - written);
        emit_bar_segment(out, &pad, STATUS_BAR_BASE_SGR, cols - written)?;
    }

    write!(out, "\x1b[0m\x1b8")?;
    if cursor_visible {
        out.write_all(b"\x1b[?25h")?;
    }
    out.write_all(b"\x1b[?2026l")?;
    out.flush()
}

/// Write one bar segment, truncated to `max_width` visible columns, and return
/// how many columns it consumed. Empty (or zero-width) content writes nothing.
fn emit_bar_segment<W: Write>(
    out: &mut W,
    text: &str,
    sgr: &str,
    max_width: usize,
) -> std::io::Result<usize> {
    if max_width == 0 {
        return Ok(0);
    }
    let piece = truncate_for_terminal(text, max_width);
    if piece.is_empty() {
        return Ok(0);
    }
    let width = UnicodeWidthStr::width(piece.as_str());
    out.write_all(color::paint(&piece, sgr).as_bytes())?;
    Ok(width)
}

/// Fetch the session list over a persistent side connection, separate from the
/// attach stream (whose responses are consumed by the event reader task). On
/// any error the connection is dropped so the next poll reconnects. Uses
/// `connect()` rather than `ensure_and_connect()`: being attached implies the
/// daemon is running, and the poll loop must never spawn one.
async fn fetch_sessions_for_bar(
    conn_slot: &mut Option<TcpConn>,
) -> anyhow::Result<Vec<SessionInfo>> {
    if conn_slot.is_none() {
        *conn_slot = Some(connect().await?);
    }
    let conn = conn_slot.as_mut().expect("connection ensured above");
    let result: anyhow::Result<Vec<SessionInfo>> = async {
        send_msg(
            conn,
            Message {
                kind: MessageKind::Request,
                id: "status-bar".to_string(),
                command: Some(Command::List),
                ..Default::default()
            },
        )
        .await?;
        let resp = recv_msg(conn).await?;
        check_response(&resp, "status-bar")?;
        Ok(resp.sessions)
    }
    .await;
    if result.is_err() {
        *conn_slot = None;
    }
    result
}

// ── Attach implementation ────────────────────────────────────────────────────

// ── Claude Code flicker hint ─────────────────────────────────────────────────

/// Path to the user-level Claude Code settings file (`~/.claude/settings.json`).
/// Uses `USERPROFILE` on Windows and `HOME` elsewhere.
fn claude_settings_path() -> Option<std::path::PathBuf> {
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))?;
    Some(
        std::path::PathBuf::from(home)
            .join(".claude")
            .join("settings.json"),
    )
}

/// Returns a colored hint when the local Claude Code settings file exists but
/// does not enable `env.CLAUDE_CODE_NO_FLICKER`. Returns `None` when the file is
/// absent/unreadable, is malformed JSON, or the flag is already enabled — in
/// those cases we stay silent rather than nag.
fn claude_no_flicker_notice() -> Option<String> {
    let content = std::fs::read_to_string(claude_settings_path()?).ok()?;
    match serde_json::from_str::<serde_json::Value>(&content) {
        Ok(value) if no_flicker_enabled_in(&value) => None,
        Ok(_) => Some(build_no_flicker_notice()),
        Err(_) => None,
    }
}

/// Whether the settings JSON already enables `env.CLAUDE_CODE_NO_FLICKER`.
/// The value is treated as enabled unless it is explicitly falsy
/// (`"0"`, `"false"`, `"no"`, `"off"`, empty, `false`, or `0`).
fn no_flicker_enabled_in(settings: &serde_json::Value) -> bool {
    let Some(value) = settings
        .get("env")
        .and_then(|e| e.get("CLAUDE_CODE_NO_FLICKER"))
    else {
        return false;
    };
    match value {
        serde_json::Value::String(s) => {
            let v = s.trim().to_ascii_lowercase();
            !(v.is_empty() || v == "0" || v == "false" || v == "no" || v == "off")
        }
        serde_json::Value::Bool(b) => *b,
        serde_json::Value::Number(n) => n.as_f64().is_some_and(|x| x != 0.0),
        _ => false,
    }
}

/// Build the colored multi-line hint. Uses `\r\n` line endings so it lays out
/// correctly under raw mode, where a bare `\n` does not return the carriage.
fn build_no_flicker_notice() -> String {
    let title = color::paint(
        "⚠  Claude Code 未启用 CLAUDE_CODE_NO_FLICKER",
        color::sgr::ERROR,
    );
    let snippet = color::paint("\"CLAUDE_CODE_NO_FLICKER\": \"1\"", color::sgr::KEY);
    let cont = color::paint("按任意键进入会话 . . .", color::sgr::HINT);
    format!(
        "{title}\r\n\r\n\
         在 qscn 会话中,Claude Code 的 PageUp/PageDown 等按键需要该设置才能正常工作。\r\n\
         强烈建议在 ~/.claude/settings.json 的 \"env\" 中加入:\r\n\r\n    \
         {snippet}\r\n\r\n{cont}"
    )
}

/// Print the hint to the alternate screen and block until any key is pressed.
/// Raw mode is already active (via `TerminalCleanupGuard`), so this reads a
/// single key event; focus/resize/mouse events are ignored.
fn show_attach_notice(notice: &str) {
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(notice.as_bytes());
    let _ = stdout.flush();
    loop {
        match crossterm::event::read() {
            Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => break,
            Ok(_) => continue,
            Err(_) => break,
        }
    }
}

async fn attach_session_loop(
    initial_session_id: &str,
    config: ClientConfig,
    mut initial_notice: Option<String>,
) -> anyhow::Result<()> {
    let mut session_id = initial_session_id.to_string();
    // Whether the current `session_id` came from an in-session switch rather than
    // the initial explicit attach. A switch target can race out between listing
    // and the handshake; when it does we recover to another live session instead
    // of exiting, while the initial explicit attach still surfaces a
    // missing/exited target unchanged.
    let mut reattaching = false;
    // The session we were attached to just before switching. It keeps running on
    // the daemon, so it is a known-good fallback if the recovery listing itself
    // fails or times out.
    let mut previous_session_id: Option<String> = None;

    loop {
        validate_session_id(&session_id)?;
        // `take()` ensures the notice is shown at most once, on the first attach;
        // switching/reattaching within the loop passes None.
        let outcome = match attach_session_once(&session_id, config, initial_notice.take()).await {
            Ok(outcome) => outcome,
            Err(err) if reattaching && err.downcast_ref::<SessionUnavailable>().is_some() => {
                // The switch target vanished before the handshake. Recover to
                // another live session so a switch never kills the client. Bound
                // the lookup so a stalled daemon cannot wedge us with the old
                // connection already dropped.
                match tokio::time::timeout(STATUS_BAR_FETCH_TIMEOUT, list_sessions()).await {
                    Ok(Ok(sessions)) => {
                        match highest_remaining_session_id(&sessions, &session_id) {
                            Some(next_session_id) => {
                                session_id = next_session_id;
                                continue;
                            }
                            // Listing succeeded and no live session remains: shut
                            // the daemon down like the normal exit path instead of
                            // leaking a sessionless daemon.
                            None => {
                                cmd_shutdown().await?;
                                return Ok(());
                            }
                        }
                    }
                    // Listing failed or timed out: fall back to the previous,
                    // still-running session if we have one; otherwise surface the
                    // original error.
                    Ok(Err(_)) | Err(_) => match previous_session_id.take() {
                        Some(prev) => {
                            session_id = prev;
                            continue;
                        }
                        None => return Err(err),
                    },
                }
            }
            Err(err) => return Err(err),
        };
        match outcome {
            AttachOutcome::SwitchTo(next_session_id) => {
                previous_session_id = Some(session_id.clone());
                session_id = next_session_id;
                reattaching = true;
            }
            // Switch after killing the current session: the session we came from
            // is dead, so it must not be kept as a reattach fallback.
            AttachOutcome::SwitchToFresh(next_session_id) => {
                previous_session_id = None;
                session_id = next_session_id;
                reattaching = true;
            }
            // Detaching leaves the daemon and all sessions running.
            AttachOutcome::Detached => return Ok(()),
            // The last session was killed from the list: shut the daemon down
            // and exit without a redundant listing so a session created in the
            // meantime cannot resurrect the client.
            AttachOutcome::Shutdown => {
                cmd_shutdown().await?;
                return Ok(());
            }
            // The attached session exited: auto-switch to the highest-ID
            // remaining session, or shut the daemon down when none remain.
            AttachOutcome::Ended => match next_session_after_exit(&session_id).await? {
                Some(next_session_id) => {
                    // The current session exited, so it is no longer a valid
                    // fallback for a later recovery.
                    previous_session_id = None;
                    session_id = next_session_id;
                    reattaching = true;
                }
                None => {
                    cmd_shutdown().await?;
                    return Ok(());
                }
            },
        }
    }
}

/// After the attached session exits, resolve the next session to attach to.
/// Queries the daemon and returns the highest-ID remaining live session, or
/// `None` when no other session remains (the caller then shuts the daemon down).
async fn next_session_after_exit(ended_session_id: &str) -> anyhow::Result<Option<String>> {
    let sessions = list_sessions().await?;
    Ok(highest_remaining_session_id(&sessions, ended_session_id))
}

/// Pick the highest numeric session ID among live (non-exited) sessions,
/// ignoring `ended_session_id`. Returns `None` when no other live session
/// remains.
fn highest_remaining_session_id(
    sessions: &[SessionInfo],
    ended_session_id: &str,
) -> Option<String> {
    sessions
        .iter()
        .filter(|s| !s.exited && s.session_id != ended_session_id)
        .filter_map(|s| {
            s.session_id
                .parse::<u64>()
                .ok()
                .map(|id| (id, s.session_id.clone()))
        })
        .max_by_key(|(id, _)| *id)
        .map(|(_, session_id)| session_id)
}

/// Pick the highest numeric session ID among live (non-exited) sessions,
/// skipping any listed in `exclude`. Returns `None` when no live session
/// outside `exclude` remains.
fn highest_live_session_id_excluding(
    sessions: &[SessionInfo],
    exclude: &[String],
) -> Option<String> {
    sessions
        .iter()
        .filter(|s| !s.exited && !exclude.iter().any(|e| e == &s.session_id))
        .filter_map(|s| {
            s.session_id
                .parse::<u64>()
                .ok()
                .map(|id| (id, s.session_id.clone()))
        })
        .max_by_key(|(id, _)| *id)
        .map(|(_, session_id)| session_id)
}

/// Resolve the session to switch to when the user presses `<prefix> n` (next)
/// or `<prefix> p` (previous). Live (non-exited) sessions are ordered by numeric
/// session ID and navigation wraps around at the ends. Returns `None` when the
/// current session is the only live one, so the caller stays put.
fn adjacent_session_id(
    sessions: &[SessionInfo],
    current_session_id: &str,
    direction: SwitchDirection,
) -> Option<String> {
    let mut live: Vec<(u64, &str)> = sessions
        .iter()
        .filter(|s| !s.exited)
        .filter_map(|s| {
            s.session_id
                .parse::<u64>()
                .ok()
                .map(|id| (id, s.session_id.as_str()))
        })
        .collect();
    live.sort_by_key(|(id, _)| *id);

    if live.len() < 2 {
        return None;
    }

    let current = live.iter().position(|(_, id)| *id == current_session_id)?;
    let len = live.len();
    let target = match direction {
        SwitchDirection::Next => (current + 1) % len,
        SwitchDirection::Prev => (current + len - 1) % len,
    };
    Some(live[target].1.to_string())
}

/// The attach handshake failed because the daemon reports the target session as
/// missing or already exited. Carries the daemon's original message so explicit
/// `attach <id>` still surfaces it verbatim, while default-attach can downcast
/// this type to fall back to another live session.
#[derive(Debug)]
struct SessionUnavailable {
    message: String,
}

impl std::fmt::Display for SessionUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for SessionUnavailable {}

async fn attach_session_once(
    session_id: &str,
    config: ClientConfig,
    notice: Option<String>,
) -> anyhow::Result<AttachOutcome> {
    let mut conn = ensure_and_connect().await?;
    let term_size = get_terminal_size().unwrap_or((80, 24));
    let (term_width, term_height) = term_size;

    let attach_id = "1";
    send_msg(
        &mut conn,
        attach_request_message(
            attach_id,
            session_id,
            term_width,
            session_area_height(config, term_height),
        ),
    )
    .await?;

    let resp = recv_msg(&mut conn).await?;
    // Distinguish "the session is gone" from other handshake failures: the daemon
    // reports a missing or already-exited session with these exact error strings.
    // Callers use this typed error to fall back to another session, while any other
    // error (connection loss, daemon crash) is surfaced unchanged.
    if resp.kind == MessageKind::Response
        && (resp.error == missing_session_error(session_id)
            || resp.error == exited_session_error(session_id))
    {
        return Err(SessionUnavailable {
            message: resp.error.clone(),
        }
        .into());
    }
    check_response(&resp, attach_id)?;

    let _terminal = TerminalCleanupGuard::enter()?;

    // Shown inside the alternate screen; the first rendered frame overwrites it,
    // which is fine — the point is that the user reads it before entering the session.
    if let Some(notice) = notice {
        show_attach_notice(&notice);
    }

    let session_id_owned = session_id.to_string();
    run_attach_loop(conn, session_id_owned, term_size, config).await
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SwitchDirection {
    Next,
    Prev,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AttachAction {
    Input(Vec<u8>),
    Resize(u16, u16),
    Focus,
    Detach,
    OpenSessionList,
    OpenHelp,
    SwitchSession(SwitchDirection),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SessionListAction {
    MoveUp,
    MoveDown,
    Select,
    Create,
    Rename,
    Kill,
    Help,
    Cancel,
    Resize(u16, u16),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AttachOutcome {
    Detached,
    SwitchTo(String),
    /// Like `SwitchTo`, but the session we came from was just killed from the
    /// list, so it must not be recorded as a reattach fallback.
    SwitchToFresh(String),
    Ended,
    /// The last session was killed from the list: shut the daemon down and exit.
    Shutdown,
}

/// After the PREFIX key is pressed, wait this long for a command key. If no key
/// arrives in time, the pending PREFIX byte is passed through to the terminal.
const PREFIX_PENDING_TIMEOUT: Duration = Duration::from_millis(1000);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct PrefixState {
    /// When the PREFIX key was pressed and is waiting for a command key.
    pending_since: Option<Instant>,
}

impl PrefixState {
    fn handle_key(
        &mut self,
        key_event: crossterm::event::KeyEvent,
        prefix: PrefixKey,
        input_modes: term::InputModeState,
        now: Instant,
    ) -> Vec<AttachAction> {
        // If the pending PREFIX has already timed out, pass it through before
        // interpreting this key so a late key is treated as normal input.
        let mut actions = self.take_expired(now, prefix);

        if self.pending_since.is_some() {
            // A key followed the PREFIX within the timeout window.
            self.pending_since = None;
            if key_char_eq_ignore_ascii_case(key_event.code, 'd') {
                actions.push(AttachAction::Detach);
                return actions;
            }
            if key_char_eq_ignore_ascii_case(key_event.code, 's') && session_list_action_enabled() {
                actions.push(AttachAction::OpenSessionList);
                return actions;
            }
            if key_char_eq_ignore_ascii_case(key_event.code, '?') {
                actions.push(AttachAction::OpenHelp);
                return actions;
            }
            if key_char_eq_ignore_ascii_case(key_event.code, 'n') {
                actions.push(AttachAction::SwitchSession(SwitchDirection::Next));
                return actions;
            }
            if key_char_eq_ignore_ascii_case(key_event.code, 'p') {
                actions.push(AttachAction::SwitchSession(SwitchDirection::Prev));
                return actions;
            }

            // Not a valid command key (a second PREFIX included): pass both the
            // PREFIX byte and this key's bytes through to the terminal.
            actions.push(AttachAction::Input(vec![prefix.byte]));
            let bytes = key_event_to_bytes(key_event, input_modes);
            if !bytes.is_empty() {
                actions.push(AttachAction::Input(bytes));
            }
            return actions;
        }

        if is_prefix_key_event(key_event, prefix) {
            self.pending_since = Some(now);
            return actions;
        }

        let bytes = key_event_to_bytes(key_event, input_modes);
        if !bytes.is_empty() {
            actions.push(AttachAction::Input(bytes));
        }
        actions
    }

    /// If a PREFIX key is pending and the timeout has elapsed, clear it and
    /// return the PREFIX byte so it is passed through to the terminal.
    fn take_expired(&mut self, now: Instant, prefix: PrefixKey) -> Vec<AttachAction> {
        if let Some(since) = self.pending_since
            && now.duration_since(since) >= PREFIX_PENDING_TIMEOUT
        {
            self.pending_since = None;
            return vec![AttachAction::Input(vec![prefix.byte])];
        }
        Vec::new()
    }
}

async fn run_attach_loop(
    conn: TcpConn,
    session_id: String,
    term_size: (u16, u16),
    config: ClientConfig,
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

    // Status bar state: session cache, cursor visibility from the last frame,
    // a persistent side connection for polling, and the row count available to
    // the application (shared with the input reader so bar-row mouse events
    // are not forwarded to the PTY).
    let mut bar_items: Vec<StatusBarItem> = Vec::new();
    let mut bar_cursor_visible = true;
    let mut bar_conn: Option<TcpConn> = None;
    let mut bar_poll = tokio::time::interval(STATUS_BAR_POLL_INTERVAL);
    bar_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let app_rows = Arc::new(AtomicU16::new(session_area_height(config, rows)));

    let writer_c = writer.clone();
    let session_id_c = session_id.clone();
    let mut msg_id: u64 = 10;

    let (action_tx, mut action_rx) = tokio::sync::mpsc::unbounded_channel::<AttachAction>();
    let stop_input = Arc::new(AtomicBool::new(false));
    let input_modes = Arc::new(std::sync::Mutex::new(frame_renderer.input_modes()));
    let mut input_handle = spawn_attach_input_reader(
        action_tx.clone(),
        stop_input.clone(),
        config.prefix,
        input_modes.clone(),
        app_rows.clone(),
    );

    let mut stdout = std::io::stdout();
    let mut pending_messages: VecDeque<Message> = VecDeque::new();
    // The most recently rendered frame, retained so overlays can repaint the
    // session locally on close instead of relying on the daemon to re-emit one.
    let mut last_frame: Option<ScreenFrame> = None;

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
                                // Repaint the bar: a force-full render (first
                                // frame, resize, alt-screen switch) clears the
                                // whole screen including the bar row.
                                bar_cursor_visible = !frame.hide_cursor;
                                let _ = draw_status_bar(
                                    &mut stdout,
                                    &bar_items,
                                    config,
                                    screen.size(),
                                    bar_cursor_visible,
                                );
                            }
                            // Keep the newest frame for overlay repaint-on-close.
                            if latest_frame.is_some() {
                                last_frame = latest_frame;
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

            _ = bar_poll.tick(), if config.status_bar => {
                match tokio::time::timeout(
                    STATUS_BAR_FETCH_TIMEOUT,
                    fetch_sessions_for_bar(&mut bar_conn),
                )
                .await
                {
                    Ok(Ok(sessions)) => {
                        let items = build_status_bar_items(&sessions, &session_id_c);
                        if items != bar_items {
                            bar_items = items;
                            let _ = draw_status_bar(
                                &mut stdout,
                                &bar_items,
                                config,
                                screen.size(),
                                bar_cursor_visible,
                            );
                        }
                    }
                    // Fetch errors already dropped the side connection; a
                    // timeout leaves a half-finished exchange behind, so drop
                    // the connection to resync on the next tick.
                    Ok(Err(_)) => {}
                    Err(_) => bar_conn = None,
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
                        let area_h = session_area_height(config, h);
                        app_rows.store(area_h, Ordering::Relaxed);
                        msg_id += 1;
                        let resize_msg = Message {
                            kind: MessageKind::Request,
                            id: msg_id.to_string(),
                            command: Some(Command::Resize),
                            session_id: session_id_c.clone(),
                            width: w as u32,
                            height: area_h as u32,
                            ..Default::default()
                        };
                        let bytes = resize_msg.to_json_line()?;
                        if writer_c.lock().await.write_all(&bytes).await.is_err() {
                            break AttachOutcome::Ended;
                        }
                        let _ = draw_status_bar(
                            &mut stdout,
                            &bar_items,
                            config,
                            (w, h),
                            bar_cursor_visible,
                        );
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

                        match run_session_list_mode(
                            &session_id_c,
                            config.prefix,
                            screen.size(),
                            &mut stdout,
                        )
                        .await?
                        {
                            SessionListSelection::Switch(next_session_id) => {
                                break AttachOutcome::SwitchTo(next_session_id);
                            }
                            SessionListSelection::SwitchFresh(next_session_id) => {
                                break AttachOutcome::SwitchToFresh(next_session_id);
                            }
                            // The last session was killed from the list: shut the
                            // daemon down and exit qscn.
                            SessionListSelection::Quit => break AttachOutcome::Shutdown,
                            SessionListSelection::Close | SessionListSelection::Error(_) => {
                                restore_session_after_overlay(
                                    &mut stdout,
                                    &mut screen,
                                    &mut frame_renderer,
                                    &input_modes,
                                    &last_frame,
                                    &bar_items,
                                    &mut bar_cursor_visible,
                                    config,
                                    &writer_c,
                                    &session_id_c,
                                    &mut msg_id,
                                    &app_rows,
                                )
                                .await;
                                stop_input.store(false, Ordering::Relaxed);
                                input_handle = spawn_attach_input_reader(
                                    action_tx.clone(),
                                    stop_input.clone(),
                                    config.prefix,
                                    input_modes.clone(),
                                    app_rows.clone(),
                                );
                            }
                        }
                    }
                    Some(AttachAction::OpenHelp) => {
                        stop_input.store(true, Ordering::Relaxed);
                        let _ = input_handle.await;

                        run_help_mode(config.prefix, screen.size(), &mut stdout).await?;
                        restore_session_after_overlay(
                            &mut stdout,
                            &mut screen,
                            &mut frame_renderer,
                            &input_modes,
                            &last_frame,
                            &bar_items,
                            &mut bar_cursor_visible,
                            config,
                            &writer_c,
                            &session_id_c,
                            &mut msg_id,
                            &app_rows,
                        )
                        .await;
                        stop_input.store(false, Ordering::Relaxed);
                        input_handle = spawn_attach_input_reader(
                            action_tx.clone(),
                            stop_input.clone(),
                            config.prefix,
                            input_modes.clone(),
                            app_rows.clone(),
                        );
                    }
                    Some(AttachAction::SwitchSession(direction)) => {
                        // Stop the input reader before the async lookup so it can
                        // never race the next attachment's reader over Crossterm
                        // events (mirrors the session-list flow).
                        stop_input.store(true, Ordering::Relaxed);
                        let _ = input_handle.await;

                        // Query the daemon for the live session set and switch to
                        // the neighbor. A bounded timeout keeps a stalled side
                        // connection from wedging the attachment. On a fetch error,
                        // timeout, or when this is the only live session, stay put.
                        let next = match tokio::time::timeout(
                            STATUS_BAR_FETCH_TIMEOUT,
                            list_sessions(),
                        )
                        .await
                        {
                            Ok(Ok(sessions)) => {
                                adjacent_session_id(&sessions, &session_id_c, direction)
                            }
                            Ok(Err(_)) | Err(_) => None,
                        };
                        if let Some(next_session_id) = next {
                            break AttachOutcome::SwitchTo(next_session_id);
                        }

                        // No switch happened: resume reading input for the current
                        // session.
                        stop_input.store(false, Ordering::Relaxed);
                        input_handle = spawn_attach_input_reader(
                            action_tx.clone(),
                            stop_input.clone(),
                            config.prefix,
                            input_modes.clone(),
                            app_rows.clone(),
                        );
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
    app_rows: Arc<AtomicU16>,
) -> tokio::task::JoinHandle<()> {
    // Keyboard/resize reading uses a bounded poll so attach cleanup can finish without process exit.
    tokio::task::spawn_blocking(move || {
        let mut prefix_state = PrefixState::default();
        while !stop_input.load(Ordering::Relaxed) {
            let event = match crossterm::event::poll(Duration::from_millis(50)) {
                Ok(true) => crossterm::event::read(),
                Ok(false) => {
                    // On idle ticks, pass through a PREFIX key that has been
                    // pending longer than the timeout.
                    for action in prefix_state.take_expired(Instant::now(), prefix) {
                        let _ = action_tx.send(action);
                    }
                    continue;
                }
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
                    for action in prefix_state.handle_key(key_event, prefix, modes, Instant::now())
                    {
                        let should_stop = matches!(
                            action,
                            AttachAction::Detach
                                | AttachAction::OpenSessionList
                                | AttachAction::OpenHelp
                                | AttachAction::SwitchSession(_)
                        );
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
                    // The status bar row is outside the session area; its
                    // coordinates would be out of range for the PTY.
                    if mouse_event.row >= app_rows.load(Ordering::Relaxed) {
                        continue;
                    }
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

/// Result of waiting on the help overlay: dismiss it, or a terminal resize.
/// The current terminal size as `(cols, rows)`, or `fallback` if it can't be
/// read. Used to restore a session at the true size after an overlay closes.
fn terminal_size_or(fallback: (u16, u16)) -> (u16, u16) {
    crossterm::terminal::size().unwrap_or(fallback)
}

/// Repaint the session view after a full-screen overlay closes. The overlay
/// cleared the screen, and a no-op resize does not make an idle app redraw, so
/// restore the last known frame locally (and the status bar) instead of relying
/// on the daemon to re-emit one. If the terminal was resized while the overlay
/// was open, the retained frame still shows the last content until the app
/// redraws — the same behavior as an ordinary resize. `frame` is `None` only when
/// no frame has arrived yet, in which case the screen is cleared.
fn repaint_session_after_overlay<W: Write>(
    stdout: &mut W,
    frame_renderer: &mut term::FrameRenderer,
    frame: Option<&ScreenFrame>,
    bar_items: &[StatusBarItem],
    config: ClientConfig,
    size: (u16, u16),
    bar_cursor_visible: &mut bool,
) {
    frame_renderer.reset();
    match frame {
        Some(frame) => {
            let _ = frame_renderer.render(stdout, frame);
            *bar_cursor_visible = !frame.hide_cursor;
        }
        // Clear so the overlay text does not linger while we wait for a frame.
        None => {
            let _ = write!(stdout, "\x1b[2J\x1b[H");
        }
    }
    let _ = draw_status_bar(stdout, bar_items, config, size, *bar_cursor_visible);
    // draw_status_bar may be a no-op (bar disabled / too short / no items), so
    // flush explicitly to guarantee the clear reaches the terminal.
    let _ = stdout.flush();
}

/// Restore the attachment after a full-screen overlay closes: reassert the
/// terminal size to the daemon and repaint the session view locally. Shared by
/// the help and session-list close paths. The caller restarts the input reader
/// afterwards, and the main loop then replays any daemon messages that arrived
/// while the overlay was open.
#[allow(clippy::too_many_arguments)]
async fn restore_session_after_overlay<S, W>(
    stdout: &mut S,
    screen: &mut term::TermScreen,
    frame_renderer: &mut term::FrameRenderer,
    input_modes: &Arc<std::sync::Mutex<term::InputModeState>>,
    last_frame: &Option<ScreenFrame>,
    bar_items: &[StatusBarItem],
    bar_cursor_visible: &mut bool,
    config: ClientConfig,
    writer: &Arc<tokio::sync::Mutex<W>>,
    session_id: &str,
    msg_id: &mut u64,
    app_rows: &Arc<AtomicU16>,
) where
    S: Write,
    W: tokio::io::AsyncWrite + Unpin,
{
    // Query the authoritative terminal size after the overlay cleared the
    // screen, so a resize while it was open does not leave the session at stale
    // dimensions.
    let (w, h) = terminal_size_or(screen.size());
    if (w, h) != screen.size() {
        screen.resize(h, w);
    }
    let area_h = session_area_height(config, h);
    app_rows.store(area_h, Ordering::Relaxed);

    *msg_id += 1;
    let resize_msg = Message {
        kind: MessageKind::Request,
        id: msg_id.to_string(),
        command: Some(Command::Resize),
        session_id: session_id.to_string(),
        width: w as u32,
        height: area_h as u32,
        ..Default::default()
    };
    if let Ok(bytes) = resize_msg.to_json_line() {
        let _ = writer.lock().await.write_all(&bytes).await;
    }

    // Repaint the last known frame to remove the overlay. Like an ordinary
    // resize (and like a broadcast frame sized for a differently-sized active
    // client), the frame renderer handles a geometry that differs from the
    // current terminal; the next frame from the daemon corrects any transient
    // mismatch.
    repaint_session_after_overlay(
        stdout,
        frame_renderer,
        last_frame.as_ref(),
        bar_items,
        config,
        (w, h),
        bar_cursor_visible,
    );
    *input_modes.lock().unwrap() = frame_renderer.input_modes();
}

/// A key/resize event read while a full-screen overlay is open. Like the
/// session-list overlay, an open overlay blocks on terminal input and does not
/// consume the daemon stream; buffered daemon messages are processed by the main
/// loop after the overlay closes.
enum HelpKey {
    Dismiss,
    Resize(u16, u16),
}

/// Wait for a key that dismisses the help screen (Esc or q) or a resize. Other
/// keys are ignored so the screen stays put.
async fn read_help_key() -> anyhow::Result<HelpKey> {
    tokio::task::spawn_blocking(|| {
        loop {
            match crossterm::event::poll(Duration::from_millis(50)) {
                Ok(true) => match crossterm::event::read() {
                    Ok(Event::Key(key_event))
                        if matches!(key_event.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                    {
                        match key_event.code {
                            KeyCode::Esc | KeyCode::Char('q') => return Ok(HelpKey::Dismiss),
                            _ => {}
                        }
                    }
                    Ok(Event::Resize(cols, rows)) => return Ok(HelpKey::Resize(cols, rows)),
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

/// Full-screen help overlay listing the in-session `<prefix>` bindings. Blocks
/// until the user presses Esc or q, re-rendering on resize so the screen tracks
/// the terminal. Returns the latest terminal size so callers that keep drawing
/// (like the session list) stay in sync with resizes that happened while the
/// help screen was open.
async fn run_help_mode<W: Write>(
    prefix: PrefixKey,
    term_size: (u16, u16),
    stdout: &mut W,
) -> anyhow::Result<(u16, u16)> {
    let mut size = term_size;
    render_help_screen(stdout, prefix, size)?;
    loop {
        match read_help_key().await? {
            HelpKey::Dismiss => return Ok(size),
            HelpKey::Resize(cols, rows) => {
                size = (cols, rows);
                render_help_screen(stdout, prefix, size)?;
            }
        }
    }
}

/// One rendered line of the help overlay body.
enum HelpBodyLine<'a> {
    Title(&'a str),
    Blank,
    Binding(&'a str, &'a str),
}

fn render_help_screen<W: Write>(
    out: &mut W,
    prefix: PrefixKey,
    term_size: (u16, u16),
) -> std::io::Result<()> {
    let (cols, rows_count) = term_size;
    let zh = is_chinese();
    write!(out, "\x1b[?2026h\x1b[0m\x1b[2J\x1b[H")?;

    let title = if zh {
        "qscreen 快捷键"
    } else {
        "qscreen key bindings"
    };
    let hint = if zh {
        "按 Esc 或 q 关闭"
    } else {
        "press Esc or q to close"
    };
    let rows = prefix_help_rows(prefix, zh);

    // Assemble the body (title, blank separator, bindings), then trim it to fit
    // above the pinned hint row so nothing scrolls off a short terminal.
    let mut body: Vec<HelpBodyLine> = Vec::with_capacity(rows.len() + 2);
    body.push(HelpBodyLine::Title(title));
    body.push(HelpBodyLine::Blank);
    for (combo, desc) in &rows {
        body.push(HelpBodyLine::Binding(combo.as_str(), desc.as_str()));
    }
    // Reserve the last terminal row for the hint.
    let budget = rows_count.saturating_sub(1) as usize;
    if body.len() > budget
        && let Some(pos) = body.iter().position(|l| matches!(l, HelpBodyLine::Blank))
    {
        // Drop the cosmetic blank separator before sacrificing any bindings.
        body.remove(pos);
    }
    body.truncate(budget);

    for line in &body {
        match line {
            HelpBodyLine::Title(t) => write_text_line(out, t, Some(color::sgr::HEADER), cols)?,
            HelpBodyLine::Blank => write_text_line(out, "", None, cols)?,
            HelpBodyLine::Binding(combo, desc) => {
                write!(out, "\x1b[0m")?;
                write_status_segments(
                    out,
                    &[(*combo, color::sgr::KEY), (*desc, color::sgr::HINT)],
                    cols,
                )?;
                write!(out, "\r\n")?;
            }
        }
    }

    // Pin the hint to the last row when there is a spare row for a gap; otherwise
    // it follows the body directly. The body never exceeds `rows_count - 1` lines,
    // so the hint always fits without scrolling.
    if (body.len() as u16) + 1 < rows_count {
        write!(out, "\x1b[{rows_count};1H")?;
    }
    write!(out, "\x1b[0m")?;
    write_status_segments(out, &[(hint, color::sgr::HINT)], cols)?;
    write!(out, "\x1b[?2026l")?;
    out.flush()
}

async fn run_session_list_mode<W: Write>(
    current_session_id: &str,
    prefix: PrefixKey,
    term_size: (u16, u16),
    stdout: &mut W,
) -> anyhow::Result<SessionListSelection> {
    let mut term_size = term_size;
    let zh = is_chinese();
    let mut rows = build_session_list_rows(&list_sessions().await?, current_session_id, zh);
    let mut selected = rows
        .iter()
        .position(|row| row.is_current)
        .unwrap_or_default();
    let mut status = String::new();

    render_session_list(stdout, &rows, selected, &status, term_size, zh)?;

    loop {
        let action = read_session_list_action().await?;
        match action {
            SessionListAction::MoveUp => {
                selected = move_session_list_selection(selected, rows.len(), -1);
                render_session_list(stdout, &rows, selected, &status, term_size, zh)?;
            }
            SessionListAction::MoveDown => {
                selected = move_session_list_selection(selected, rows.len(), 1);
                render_session_list(stdout, &rows, selected, &status, term_size, zh)?;
            }
            SessionListAction::Cancel => return Ok(SessionListSelection::Close),
            SessionListAction::Help => {
                // The help overlay owns the screen until dismissed, then the
                // list repaints so the user comes back where they left off.
                term_size = run_help_mode(prefix, term_size, stdout).await?;
                render_session_list(stdout, &rows, selected, &status, term_size, zh)?;
            }
            SessionListAction::Resize(cols, rows_count) => {
                term_size = (cols, rows_count);
                render_session_list(stdout, &rows, selected, &status, term_size, zh)?;
            }
            SessionListAction::Create => {
                // Inherit the cwd of the session under the cursor so the new session opens in the same directory;
                // if that session has no known cwd, fall back to create_session's default behavior (the client's current directory).
                let inherit_cwd = rows.get(selected).map(|row| row.cwd_raw.clone());
                match create_session_in(inherit_cwd.as_deref()).await {
                    Ok(new_session_id) => return Ok(SessionListSelection::Switch(new_session_id)),
                    Err(e) => {
                        status = if zh {
                            format!("新建失败: {e}")
                        } else {
                            format!("create failed: {e}")
                        };
                        render_session_list(stdout, &rows, selected, &status, term_size, zh)?;
                    }
                }
            }
            SessionListAction::Rename => {
                if rows.is_empty() {
                    status = if zh { "无会话" } else { "no sessions" }.to_string();
                    render_session_list(stdout, &rows, selected, &status, term_size, zh)?;
                    continue;
                }
                selected = selected.min(rows.len() - 1);
                let row = rows[selected].clone();
                if row.exited {
                    status = if zh {
                        "无法重命名已退出的会话"
                    } else {
                        "cannot rename an exited session"
                    }
                    .to_string();
                    render_session_list(stdout, &rows, selected, &status, term_size, zh)?;
                    continue;
                }
                match prompt_session_rename(stdout, &rows, selected, &mut term_size, &row.name, zh)
                    .await?
                {
                    Some(new_name) => match rename_session(&row.session_id, &new_name).await {
                        Ok(()) => {
                            rows = build_session_list_rows(
                                &list_sessions().await?,
                                current_session_id,
                                zh,
                            );
                            selected = rows
                                .iter()
                                .position(|r| r.session_id == row.session_id)
                                .unwrap_or_else(|| selected.min(rows.len().saturating_sub(1)));
                            status = if zh {
                                format!("已重命名为 \"{new_name}\"")
                            } else {
                                format!("renamed to \"{new_name}\"")
                            };
                        }
                        Err(e) => {
                            status = if zh {
                                format!("重命名失败: {e}")
                            } else {
                                format!("rename failed: {e}")
                            }
                        }
                    },
                    None => status = String::new(),
                }
                render_session_list(stdout, &rows, selected, &status, term_size, zh)?;
            }
            SessionListAction::Kill => {
                if rows.is_empty() {
                    status = if zh { "无会话" } else { "no sessions" }.to_string();
                    render_session_list(stdout, &rows, selected, &status, term_size, zh)?;
                    continue;
                }
                selected = selected.min(rows.len() - 1);
                let row = rows[selected].clone();
                if !prompt_session_kill_confirm(stdout, &rows, selected, &mut term_size, &row, zh)
                    .await?
                {
                    status = String::new();
                    render_session_list(stdout, &rows, selected, &status, term_size, zh)?;
                    continue;
                }
                match cmd_kill(&row.session_id).await {
                    Ok(()) => {
                        let sessions = list_sessions().await?;
                        rows = build_session_list_rows(&sessions, current_session_id, zh);
                        if rows.is_empty() {
                            // The last session is gone: exit qscn and let the
                            // daemon shut down like the normal no-sessions path.
                            return Ok(SessionListSelection::Quit);
                        }
                        if row.session_id == current_session_id {
                            // The attached session was just killed, so we cannot
                            // return to it. Switch to the highest-ID live
                            // session, or quit when only exited sessions remain
                            // (nothing live is left to attach to).
                            match highest_remaining_session_id(&sessions, &row.session_id) {
                                Some(next_session_id) => {
                                    return Ok(SessionListSelection::SwitchFresh(next_session_id));
                                }
                                None => return Ok(SessionListSelection::Quit),
                            }
                        }
                        selected = selected.min(rows.len() - 1);
                        status = if zh {
                            format!("已终止 \"{}\"", row.name)
                        } else {
                            format!("killed \"{}\"", row.name)
                        };
                    }
                    Err(e) => {
                        status = if zh {
                            format!("终止失败: {e}")
                        } else {
                            format!("kill failed: {e}")
                        }
                    }
                }
                render_session_list(stdout, &rows, selected, &status, term_size, zh)?;
            }
            SessionListAction::Select => {
                if rows.is_empty() {
                    status = if zh { "无会话" } else { "no sessions" }.to_string();
                    render_session_list(stdout, &rows, selected, &status, term_size, zh)?;
                    continue;
                }

                rows = build_session_list_rows(&list_sessions().await?, current_session_id, zh);
                if rows.is_empty() {
                    selected = 0;
                    status = if zh { "无会话" } else { "no sessions" }.to_string();
                    render_session_list(stdout, &rows, selected, &status, term_size, zh)?;
                    continue;
                }
                selected = selected.min(rows.len().saturating_sub(1));
                let selection = selection_for_session_row(&rows[selected]);
                match selection {
                    SessionListSelection::Close => return Ok(SessionListSelection::Close),
                    SessionListSelection::Switch(name) => {
                        return Ok(SessionListSelection::Switch(name));
                    }
                    // selection_for_session_row only yields Close/Switch/Error;
                    // propagate the kill-only variants for match exhaustiveness.
                    SessionListSelection::SwitchFresh(name) => {
                        return Ok(SessionListSelection::SwitchFresh(name));
                    }
                    SessionListSelection::Quit => return Ok(SessionListSelection::Quit),
                    SessionListSelection::Error(error) => {
                        status = error.clone();
                        render_session_list(stdout, &rows, selected, &status, term_size, zh)?;
                    }
                }
            }
        }
    }
}

/// Map a key press to a session-list navigation action, or `None` if the key is
/// not bound.
fn map_session_list_key(code: KeyCode) -> Option<SessionListAction> {
    match code {
        KeyCode::Up | KeyCode::Char('k') => Some(SessionListAction::MoveUp),
        KeyCode::Down | KeyCode::Char('j') => Some(SessionListAction::MoveDown),
        KeyCode::Enter => Some(SessionListAction::Select),
        KeyCode::Char('c') => Some(SessionListAction::Create),
        KeyCode::Char('r') => Some(SessionListAction::Rename),
        KeyCode::Char('x') => Some(SessionListAction::Kill),
        KeyCode::Char('?') => Some(SessionListAction::Help),
        KeyCode::Esc | KeyCode::Char('q') => Some(SessionListAction::Cancel),
        _ => None,
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
                        if let Some(action) = map_session_list_key(key_event.code) {
                            return Ok(action);
                        }
                    }
                    Ok(Event::Resize(cols, rows)) => {
                        return Ok(SessionListAction::Resize(cols, rows));
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

#[derive(Debug, Clone, PartialEq, Eq)]
enum NameEditKey {
    Char(char),
    Backspace,
    Submit,
    Cancel,
    Resize(u16, u16),
}

/// Map a key press to a rename-prompt edit action, or `None` if the key is not
/// bound.
fn map_name_edit_key(code: KeyCode) -> Option<NameEditKey> {
    match code {
        KeyCode::Enter => Some(NameEditKey::Submit),
        KeyCode::Esc => Some(NameEditKey::Cancel),
        KeyCode::Backspace => Some(NameEditKey::Backspace),
        KeyCode::Char(c) => Some(NameEditKey::Char(c)),
        _ => None,
    }
}

async fn read_name_edit_key() -> anyhow::Result<NameEditKey> {
    tokio::task::spawn_blocking(|| {
        loop {
            match crossterm::event::poll(Duration::from_millis(50)) {
                Ok(true) => match crossterm::event::read() {
                    Ok(Event::Key(key_event))
                        if matches!(key_event.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                    {
                        if let Some(action) = map_name_edit_key(key_event.code) {
                            return Ok(action);
                        }
                    }
                    Ok(Event::Resize(cols, rows)) => {
                        return Ok(NameEditKey::Resize(cols, rows));
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

/// Prompt the user for a new name inline in the session list's bottom row.
/// Returns Some(name) on submit, None on cancel. `term_size` is updated in
/// place on resize so the caller keeps rendering at the live terminal size.
async fn prompt_session_rename<W: Write>(
    stdout: &mut W,
    rows: &[SessionListRow],
    selected: usize,
    term_size: &mut (u16, u16),
    old_name: &str,
    zh: bool,
) -> anyhow::Result<Option<String>> {
    let mut input = String::new();
    loop {
        // Split the prompt into three color segments so the instruction, the
        // user's input, and the trailing hint each stand out and are visually
        // distinct: the label is prominent (yellow), the typed input uses a
        // separate prominent color (cyan) with a `_` caret, and the key hint
        // stays dim. Concatenated their text matches the original single line.
        let label = if zh {
            format!("重命名 \"{old_name}\" -> ")
        } else {
            format!("rename \"{old_name}\" -> ")
        };
        let hint = if zh {
            "  (Enter 确认, Esc 取消)"
        } else {
            "  (Enter confirm, Esc cancel)"
        };
        let input_field = format!("{input}_");
        let segments = [
            (label.as_str(), color::sgr::PROMPT),
            (input_field.as_str(), color::sgr::INPUT),
            (hint, color::sgr::HINT),
        ];
        render_session_list_with_status(stdout, rows, selected, &segments, *term_size, zh)?;
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
            NameEditKey::Resize(cols, rows_count) => {
                *term_size = (cols, rows_count);
            }
        }
    }
}

/// Ask the user to confirm killing the selected session, inline in the session
/// list's bottom row. Returns true only on an explicit `y`/`Y`; every other key
/// (Esc, Enter, `n`, or any stray key) cancels so the destructive action fails
/// closed. `term_size` is updated in place on resize so the caller keeps
/// rendering at the live terminal size.
async fn prompt_session_kill_confirm<W: Write>(
    stdout: &mut W,
    rows: &[SessionListRow],
    selected: usize,
    term_size: &mut (u16, u16),
    row: &SessionListRow,
    zh: bool,
) -> anyhow::Result<bool> {
    loop {
        // The label uses the error color to flag the destructive action; the
        // key hint stays dim like the rename prompt.
        let label = if zh {
            format!("终止 \"{}\" (会话 {})? ", row.name, row.session_id)
        } else {
            format!("kill \"{}\" (session {})? ", row.name, row.session_id)
        };
        let hint = if zh {
            "(y 确认, Esc 取消)"
        } else {
            "(y confirm, Esc cancel)"
        };
        let segments = [
            (label.as_str(), color::sgr::ERROR),
            (hint, color::sgr::HINT),
        ];
        render_session_list_with_status(stdout, rows, selected, &segments, *term_size, zh)?;
        match read_kill_confirm_key().await? {
            ConfirmKey::Confirm => return Ok(true),
            ConfirmKey::Cancel => return Ok(false),
            ConfirmKey::Resize(cols, rows_count) => {
                *term_size = (cols, rows_count);
            }
        }
    }
}

/// A key read at the kill-confirmation prompt.
enum ConfirmKey {
    Confirm,
    Cancel,
    Resize(u16, u16),
}

/// Read a key at the kill-confirmation prompt. Only a plain `y`/`Y` (optionally
/// with Shift) confirms; a Ctrl/Alt-modified key or any other key cancels so the
/// destructive action fails closed.
async fn read_kill_confirm_key() -> anyhow::Result<ConfirmKey> {
    tokio::task::spawn_blocking(|| {
        loop {
            match crossterm::event::poll(Duration::from_millis(50)) {
                Ok(true) => match crossterm::event::read() {
                    Ok(Event::Key(key_event))
                        if matches!(key_event.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                    {
                        // Shift is allowed (it produces `Y`); any other modifier
                        // (Ctrl/Alt/Super/Hyper/Meta) cancels.
                        let has_extra_modifier = !key_event
                            .modifiers
                            .difference(KeyModifiers::SHIFT)
                            .is_empty();
                        match key_event.code {
                            KeyCode::Char('y') | KeyCode::Char('Y') if !has_extra_modifier => {
                                return Ok(ConfirmKey::Confirm);
                            }
                            _ => return Ok(ConfirmKey::Cancel),
                        }
                    }
                    Ok(Event::Resize(cols, rows)) => return Ok(ConfirmKey::Resize(cols, rows)),
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
    zh: bool,
) -> std::io::Result<()> {
    // The whole status line shares one content-derived color; the empty status
    // falls back to the dim "* marks current session" hint.
    let (text, sgr) = if status.is_empty() {
        let marker = if zh {
            "* 表示当前会话"
        } else {
            "* marks current session"
        };
        (marker, color::sgr::HINT)
    } else {
        (status, status_style(status))
    };
    render_session_list_with_status(out, rows, selected, &[(text, sgr)], term_size, zh)
}

/// Same as [`render_session_list`], but the bottom status line is a sequence of
/// independently colored `(text, sgr)` segments. Segments are laid out left to
/// right and truncated together by visible width, so callers can highlight parts
/// of the line differently (e.g. the rename prompt label vs. the user's input).
fn render_session_list_with_status<W: Write>(
    out: &mut W,
    rows: &[SessionListRow],
    selected: usize,
    status_segments: &[(&str, &str)],
    term_size: (u16, u16),
    zh: bool,
) -> std::io::Result<()> {
    let (cols, rows_count) = term_size;
    write!(out, "\x1b[?2026h\x1b[0m\x1b[2J\x1b[H")?;

    // Reserve the terminal's bottom row for the status line; the header and
    // the session rows share the remaining budget so nothing ever scrolls off
    // a short terminal.
    let budget = rows_count.saturating_sub(1) as usize;
    let body_len = rows.len().max(1); // an empty list still renders "no sessions"
    let header_len = session_list_header_len(budget, body_len);
    let (start, visible) =
        session_list_viewport(budget.saturating_sub(header_len), body_len, selected);

    if header_len >= 1 {
        let title = if zh {
            "qscreen 会话列表"
        } else {
            "qscreen sessions"
        };
        write_text_line(out, title, Some(color::sgr::HEADER), cols)?;
    }
    if header_len >= 2 {
        write_session_list_hint(out, cols, zh)?;
    }
    if header_len >= 3 {
        write_text_line(out, "", None, cols)?;
    }

    if rows.is_empty() {
        if visible > 0 {
            let empty = if zh { "  无会话" } else { "  no sessions" };
            write_text_line(out, empty, Some(color::sgr::HINT), cols)?;
        }
    } else {
        for (idx, row) in rows.iter().enumerate().skip(start).take(visible) {
            write_session_row(out, row, idx == selected, cols)?;
        }
    }

    // The status line always sits on the bottom row and carries no CRLF. The
    // header and body never exceed `rows_count - 1` lines, so this never
    // scrolls.
    write!(out, "\x1b[{};1H", rows_count.max(1))?;
    write!(out, "\x1b[0m")?;
    write_status_segments(out, status_segments, cols)?;
    write!(out, "\x1b[?2026l")?;
    out.flush()
}

/// Number of session-list header lines (title, hint, blank separator) that fit
/// alongside `body_len` body lines in `budget` lines. On short terminals the
/// decorations drop before any session rows: the blank separator first, then
/// the hint, then the title.
fn session_list_header_len(budget: usize, body_len: usize) -> usize {
    let mut header_len = 3usize;
    while header_len > 0 && header_len + body_len > budget {
        header_len -= 1;
    }
    header_len
}

/// Window of session rows to draw as `(start, len)`: `selected` always falls
/// inside the window and the window never exceeds `budget` lines. When the
/// list is scrolled, the selected row sticks to the window's bottom edge.
fn session_list_viewport(budget: usize, len: usize, selected: usize) -> (usize, usize) {
    let visible = budget.min(len);
    if visible == 0 {
        return (0, 0);
    }
    let selected = selected.min(len - 1);
    let start = selected.saturating_sub(visible - 1).min(len - visible);
    (start, visible)
}

/// Write the bottom status line as consecutive colored segments, truncating them
/// together to `cols` visible columns and right-padding the remainder with
/// spaces. Colors are only emitted when the terminal supports them; otherwise
/// the visible width (and thus the layout) is identical.
fn write_status_segments<W: Write>(
    out: &mut W,
    segments: &[(&str, &str)],
    cols: u16,
) -> std::io::Result<()> {
    let limit = cols as usize;
    let colored = color::supported();
    let mut used = 0usize;
    for (text, sgr) in segments {
        if used >= limit {
            break;
        }
        let piece = truncate_for_terminal(text, limit - used);
        let width = UnicodeWidthStr::width(piece.as_str());
        // The segment was truncated (its full width did not fit). Emit what fits,
        // then stop: continuing into later segments could render text past the
        // cut point — and if a wide glyph was dropped at the boundary, leak a
        // later segment into the leftover column, breaking the visible prefix.
        let truncated = width < UnicodeWidthStr::width(*text);
        if width > 0 {
            if colored {
                write!(out, "\x1b[{sgr}m{piece}\x1b[0m")?;
            } else {
                write!(out, "{piece}")?;
            }
            used += width;
        }
        if truncated {
            break;
        }
    }
    for _ in used..limit {
        out.write_all(b" ")?;
    }
    Ok(())
}

/// Pick the status line color by content: errors=red, successful rename=green,
/// everything else=dim hint. Matches both the English and Chinese status
/// messages produced by the session list.
fn status_style(status: &str) -> &'static str {
    let lower = status.to_ascii_lowercase();
    let is_error = lower.contains("fail")
        || lower.contains("cannot")
        || lower.contains("error")
        || lower.contains("no sessions")
        || status.contains("失败")
        || status.contains("无法")
        || status.contains("无会话");
    let is_success = lower.starts_with("renamed") || status.starts_with("已重命名");
    if is_error {
        color::sgr::ERROR
    } else if is_success {
        color::sgr::SUCCESS
    } else {
        color::sgr::HINT
    }
}

/// The session list's action hint line: highlight shortcut fragments (bold yellow) while keeping description text dim.
/// In a colorless environment it degrades to plain text with the same visible width, so the layout is unaffected.
fn write_session_list_hint<W: Write>(out: &mut W, cols: u16, zh: bool) -> std::io::Result<()> {
    // (fragment text, whether it is a shortcut key). Concatenated they form the
    // hint string, so it can be truncated by visible width. The shortcut keys
    // stay identical across languages; only the descriptions are translated.
    const SEGMENTS_EN: &[(&str, bool)] = &[
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
        ("x", true),
        (" kill, ", false),
        ("?", true),
        (" help, ", false),
        ("Esc/q", true),
        (" cancel", false),
    ];
    const SEGMENTS_ZH: &[(&str, bool)] = &[
        ("Up/Down", true),
        (" 或 ", false),
        ("k/j", true),
        (", ", false),
        ("Enter", true),
        (" 切换, ", false),
        ("c", true),
        (" 新建, ", false),
        ("r", true),
        (" 改名, ", false),
        ("x", true),
        (" 终止, ", false),
        ("?", true),
        (" 帮助, ", false),
        ("Esc/q", true),
        (" 取消", false),
    ];
    let segments = if zh { SEGMENTS_ZH } else { SEGMENTS_EN };
    let limit = cols as usize;
    let mut content = String::new();
    let mut visible = 0usize;
    for (text, is_key) in segments {
        if visible >= limit {
            break;
        }
        let piece = truncate_for_terminal(text, limit - visible);
        let piece_width = UnicodeWidthStr::width(piece.as_str());
        // If a segment does not fit whole (e.g. a two-column CJK glyph with only
        // one column left), emit what fits and stop rather than skipping ahead to
        // a later, narrower segment, which would render the keys out of order.
        let truncated = piece_width < UnicodeWidthStr::width(*text);
        if piece_width > 0 {
            visible += piece_width;
            let sgr = if *is_key {
                color::sgr::KEY
            } else {
                color::sgr::HINT
            };
            // color::paint returns text unchanged in a colorless environment, so this branch covers both the colored and colorless cases.
            content.push_str(&color::paint(&piece, sgr));
        }
        if truncated {
            break;
        }
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
            "{} {} {:<4} {:<24} {} {:>8}  {}",
            selector,
            current,
            truncate_for_terminal(&row.session_id, 4),
            truncate_for_terminal(&row.name, 24),
            pad_end_to_width(&truncate_for_terminal(&row.state, 14), 14),
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
    // The state label may contain wide CJK glyphs when translated, so pad it by
    // display width to keep the fixed 14-column layout (ROW_PREFIX_WIDTH) intact.
    let state = pad_end_to_width(&truncate_for_terminal(&row.state, 14), 14);
    let size = truncate_for_terminal(&row.size, 8);
    let cwd = truncate_for_terminal(&row.cwd, cwd_budget);
    let visible = ROW_PREFIX_WIDTH + UnicodeWidthStr::width(cwd.as_str());

    if selected {
        // Reverse-video the whole line; do not also apply per-column foreground colors, to avoid interfering with the reverse video.
        let current = if row.is_current { "*" } else { " " };
        let content = format!(
            "{} {} {:<4} {:<24} {} {:>8}  {}",
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
            color::paint(&state, color::state_sgr(row.exited, row.attached)),
            color::paint(&format!("{size:>8}"), color::sgr::SIZE),
            color::paint(&cwd, color::sgr::CWD),
        );
        write_list_line(out, &content, visible, None, cols)
    } else {
        let current = if row.is_current { "*" } else { " " };
        let content = format!(
            "{} {} {:<4} {:<24} {} {:>8}  {}",
            selector, current, id, name, state, size, cwd
        );
        write_list_line(out, &content, visible, None, cols)
    }
}

/// Right-pad `value` with spaces to at least `width` display columns. Unlike
/// `format!("{value:<width$}")`, which counts `char`s, this counts display width
/// so columns stay aligned when the text contains wide (CJK) glyphs, such as the
/// translated session state labels. For pure-ASCII text it is byte-identical to
/// the `{:<width}` formatting it replaces.
fn pad_end_to_width(value: &str, width: usize) -> String {
    let visible = UnicodeWidthStr::width(value);
    if visible >= width {
        return value.to_string();
    }
    let mut padded = String::with_capacity(value.len() + (width - visible));
    padded.push_str(value);
    for _ in visible..width {
        padded.push(' ');
    }
    padded
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

    fn no_flicker_enabled(json: &str) -> bool {
        no_flicker_enabled_in(&serde_json::from_str(json).unwrap())
    }

    #[test]
    fn no_flicker_detects_enabled_string_one() {
        assert!(no_flicker_enabled(
            r#"{"env":{"CLAUDE_CODE_NO_FLICKER":"1"}}"#
        ));
        assert!(no_flicker_enabled(
            r#"{"env":{"CLAUDE_CODE_NO_FLICKER":"true"}}"#
        ));
        assert!(no_flicker_enabled(
            r#"{"env":{"CLAUDE_CODE_NO_FLICKER":true}}"#
        ));
    }

    #[test]
    fn no_flicker_missing_or_falsy_is_not_enabled() {
        assert!(!no_flicker_enabled(r#"{}"#));
        assert!(!no_flicker_enabled(r#"{"env":{}}"#));
        assert!(!no_flicker_enabled(r#"{"env":{"OTHER":"1"}}"#));
        assert!(!no_flicker_enabled(
            r#"{"env":{"CLAUDE_CODE_NO_FLICKER":"0"}}"#
        ));
        assert!(!no_flicker_enabled(
            r#"{"env":{"CLAUDE_CODE_NO_FLICKER":""}}"#
        ));
        assert!(!no_flicker_enabled(
            r#"{"env":{"CLAUDE_CODE_NO_FLICKER":"off"}}"#
        ));
        assert!(!no_flicker_enabled(
            r#"{"env":{"CLAUDE_CODE_NO_FLICKER":false}}"#
        ));
    }

    #[test]
    fn parse_prefix_accepts_supported_aliases() {
        let ctrl_a = PrefixKey {
            ctrl_char: 'A',
            byte: 0x01,
        };
        assert_eq!(PrefixKey::parse("C-a").unwrap(), ctrl_a);
        assert_eq!(PrefixKey::parse("c-a").unwrap(), ctrl_a);
        assert_eq!(PrefixKey::parse("Ctrl+A").unwrap(), ctrl_a);
        assert_eq!(PrefixKey::parse("C-b").unwrap(), DEFAULT_PREFIX);
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
        let (config, args) = parse_client_config_with_env(vec![], None, None).unwrap();
        assert_eq!(config.prefix, DEFAULT_PREFIX);
        assert!(config.status_bar);
        assert!(args.is_empty());
    }

    #[test]
    fn client_config_uses_environment_fallback() {
        let (config, args) = parse_client_config_with_env(
            vec!["attach".into(), "work".into()],
            Some("C-b".into()),
            None,
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
    fn client_config_cli_overrides_environment() {
        let (config, args) = parse_client_config_with_env(
            vec![
                "--prefix".into(),
                "C-b".into(),
                "attach".into(),
                "work".into(),
            ],
            Some("C-a".into()),
            None,
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
            let (config, remaining) = parse_client_config_with_env(
                args.iter().map(|s| s.to_string()).collect(),
                None,
                None,
            )
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
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(err.starts_with("invalid prefix"), "{err}");

        let err = parse_client_config_with_env(
            vec!["attach".into(), "work".into()],
            Some("C-1".into()),
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(err.starts_with("invalid prefix"), "{err}");
    }

    #[test]
    fn client_config_status_bar_env_and_cli() {
        // Env alone turns the bar off.
        let (config, _) = parse_client_config_with_env(vec![], None, Some("off".into())).unwrap();
        assert!(!config.status_bar);

        // CLI takes precedence over env, and the option is stripped from args.
        let (config, args) = parse_client_config_with_env(
            vec![
                "--status-bar".into(),
                "on".into(),
                "attach".into(),
                "1".into(),
            ],
            None,
            Some("off".into()),
        )
        .unwrap();
        assert!(config.status_bar);
        assert_eq!(args, vec!["attach", "1"]);

        // `--status-bar=off` form works too.
        let (config, args) =
            parse_client_config_with_env(vec!["--status-bar=off".into()], None, None).unwrap();
        assert!(!config.status_bar);
        assert!(args.is_empty());
    }

    #[test]
    fn client_config_rejects_invalid_status_bar_value() {
        let err = parse_client_config_with_env(vec!["--status-bar=maybe".into()], None, None)
            .unwrap_err()
            .to_string();
        assert!(err.starts_with("invalid status bar option"), "{err}");

        let err = parse_client_config_with_env(vec!["--status-bar".into()], None, None)
            .unwrap_err()
            .to_string();
        assert!(err.starts_with("invalid status bar option"), "{err}");
    }

    fn test_config(status_bar: bool) -> ClientConfig {
        ClientConfig {
            prefix: DEFAULT_PREFIX,
            status_bar,
        }
    }

    #[test]
    fn session_area_height_reserves_bottom_row_when_active() {
        assert_eq!(session_area_height(test_config(true), 24), 23);
        assert_eq!(
            session_area_height(test_config(true), STATUS_BAR_MIN_HEIGHT),
            STATUS_BAR_MIN_HEIGHT - 1
        );
        // Too small for the bar: keep every row for the session.
        assert_eq!(session_area_height(test_config(true), 2), 2);
        assert_eq!(session_area_height(test_config(true), 1), 1);
        // Disabled by config: full height regardless.
        assert_eq!(session_area_height(test_config(false), 24), 24);
    }

    fn bar_session(session_id: &str, name: &str, exited: bool, attached: bool) -> SessionInfo {
        SessionInfo {
            session_id: session_id.to_string(),
            name: name.to_string(),
            exited,
            attached,
            ..Default::default()
        }
    }

    #[test]
    fn status_bar_items_sort_numerically_mark_current_and_drop_exited() {
        let sessions = vec![
            bar_session("10", "ten", false, false),
            bar_session("2", "two", false, true),
            bar_session("1", "one", true, false),
            bar_session("3", "three", false, false),
        ];

        let items = build_status_bar_items(&sessions, "2");

        // Exited session "1" is dropped; the rest sort numerically.
        assert_eq!(
            items
                .iter()
                .map(|item| item.session_id.as_str())
                .collect::<Vec<_>>(),
            vec!["2", "3", "10"]
        );
        assert!(items[0].is_current);
        assert!(!items[1].is_current);
    }

    #[test]
    fn status_bar_segments_carry_current_marker_and_omit_exited() {
        let items = build_status_bar_items(
            &[
                bar_session("1", "work", false, true),
                bar_session("2", "next", false, false),
                bar_session("3", "old", true, false),
            ],
            "1",
        );

        let text: String = status_bar_segments(&items)
            .iter()
            .map(|(text, _)| text.as_str())
            .collect();

        // Current session marks `*` on the left of the id; the exited session
        // "3:old" is not listed at all.
        assert_eq!(text, "[qscn] *1:work 2:next");
    }

    #[test]
    fn render_status_bar_writes_bottom_row_and_restores_cursor() {
        let items = build_status_bar_items(&[bar_session("1", "work", false, true)], "1");
        let cols = 20u16;
        let mut out = Vec::new();

        render_status_bar(&mut out, &items, (cols, 5), true).unwrap();

        let text = String::from_utf8(out).unwrap();
        // Bottom row addressing, cursor save/restore, and synchronized update.
        assert!(text.contains("\x1b[5;1H"), "{text:?}");
        assert!(text.contains("\x1b7"), "{text:?}");
        assert!(text.contains("\x1b8"), "{text:?}");
        assert!(text.starts_with("\x1b[?2026h"), "{text:?}");
        assert!(text.ends_with("\x1b[?2026l"), "{text:?}");
        assert!(text.contains("\x1b[?25h"), "{text:?}");
        // Colorless test environment: plain content padded to the full width.
        let start = text.find("\x1b[5;1H").unwrap() + "\x1b[5;1H".len();
        let end = text.find("\x1b[0m\x1b8").unwrap();
        let content = &text[start..end];
        assert_eq!(content.len(), cols as usize, "{content:?}");
        assert!(content.starts_with("[qscn] *1:work"), "{content:?}");
    }

    #[test]
    fn render_status_bar_hidden_cursor_stays_hidden() {
        let items = build_status_bar_items(&[bar_session("1", "work", false, true)], "1");
        let mut out = Vec::new();

        render_status_bar(&mut out, &items, (20, 5), false).unwrap();

        let text = String::from_utf8(out).unwrap();
        assert!(!text.contains("\x1b[?25h"), "{text:?}");
    }

    #[test]
    fn render_status_bar_truncates_to_narrow_width() {
        let items = build_status_bar_items(
            &[
                bar_session("1", "alpha", false, true),
                bar_session("2", "beta", false, false),
            ],
            "1",
        );
        let cols = 10u16;
        let mut out = Vec::new();

        render_status_bar(&mut out, &items, (cols, 5), true).unwrap();

        let text = String::from_utf8(out).unwrap();
        let start = text.find("\x1b[5;1H").unwrap() + "\x1b[5;1H".len();
        let end = text.find("\x1b[0m\x1b8").unwrap();
        let content = &text[start..end];
        // Too narrow for all sessions: the items region scrolls to keep the
        // current session (`*1:alpha`) anchored, so its start shows right after
        // the pinned header even though the separator is scrolled off.
        assert_eq!(content, "[qscn]*1:a");
    }

    #[test]
    fn status_bar_scroll_offset_zero_when_everything_fits() {
        assert_eq!(status_bar_scroll_offset(10, 20, Some((3, 7))), 0);
        assert_eq!(status_bar_scroll_offset(20, 20, Some((3, 7))), 0);
    }

    #[test]
    fn status_bar_scroll_offset_keeps_current_tail_visible() {
        // total 30, region 10, current occupies [24, 29): scroll so end <= offset+region.
        let offset = status_bar_scroll_offset(30, 10, Some((24, 29)));
        assert_eq!(offset, 19); // end(29) - region(10)
        assert!(24 >= offset && 29 <= offset + 10, "offset={offset}");
    }

    #[test]
    fn status_bar_scroll_offset_aligns_start_when_current_wider_than_region() {
        // current is 12 columns wide but the region is only 8: show from its start.
        assert_eq!(status_bar_scroll_offset(40, 8, Some((5, 17))), 5);
    }

    #[test]
    fn render_status_bar_scrolls_to_keep_current_name_visible() {
        let mut sessions = Vec::new();
        for i in 1..=6 {
            sessions.push(bar_session(
                &i.to_string(),
                &format!("session{i}"),
                false,
                false,
            ));
        }
        // Current session is the last one, so without scrolling it would fall off
        // the right edge of a narrow bar.
        let items = build_status_bar_items(&sessions, "6");
        let cols = 24u16;
        let mut out = Vec::new();

        render_status_bar(&mut out, &items, (cols, 5), true).unwrap();

        let text = String::from_utf8(out).unwrap();
        let start = text.find("\x1b[5;1H").unwrap() + "\x1b[5;1H".len();
        let end = text.find("\x1b[0m\x1b8").unwrap();
        let content = &text[start..end];
        assert_eq!(
            UnicodeWidthStr::width(content),
            cols as usize,
            "{content:?}"
        );
        assert!(content.starts_with("[qscn]"), "{content:?}");
        assert!(content.contains("*6:session6"), "{content:?}");
    }

    #[test]
    fn session_list_hint_renders_full_text_and_pads_to_width() {
        // In a colorless environment (the test default), the hint line should be plain text, right-padded with spaces to fill the whole line.
        let cols = 80u16;
        let mut out = Vec::new();
        write_session_list_hint(&mut out, cols, false).unwrap();
        let rendered = String::from_utf8(out).unwrap();
        assert!(rendered.ends_with("\r\n"));
        let line = rendered.strip_suffix("\r\n").unwrap();
        assert!(
            line.contains(
                "Up/Down or k/j, Enter switch, c create, r rename, x kill, ? help, Esc/q cancel"
            ),
            "{line:?}"
        );
        // After stripping any leading SGR reset prefix, the visible content should exactly fill cols columns.
        let visible = line.trim_start_matches("\x1b[0m");
        assert_eq!(UnicodeWidthStr::width(visible), cols as usize);
    }

    #[test]
    fn session_list_hint_renders_chinese_and_pads_to_width() {
        // The Chinese hint keeps the shortcut keys but translates the descriptions,
        // and still pads to the full terminal width.
        let cols = 80u16;
        let mut out = Vec::new();
        write_session_list_hint(&mut out, cols, true).unwrap();
        let rendered = String::from_utf8(out).unwrap();
        let line = rendered.strip_suffix("\r\n").unwrap();
        assert!(
            line.contains("Up/Down 或 k/j, Enter 切换, c 新建, r 改名, x 终止, ? 帮助, Esc/q 取消"),
            "{line:?}"
        );
        let visible = line.trim_start_matches("\x1b[0m");
        assert_eq!(UnicodeWidthStr::width(visible), cols as usize);
    }

    #[test]
    fn session_list_hint_chinese_truncates_in_order() {
        // At a width that cuts a two-column glyph, the hint must stop in order and
        // never pull a later key forward, so the visible text (minus padding) stays
        // a prefix of the full hint string.
        const FULL: &str = "Up/Down 或 k/j, Enter 切换, c 新建, r 改名, x 终止, ? 帮助, Esc/q 取消";
        let cols = 23u16;
        let mut out = Vec::new();
        write_session_list_hint(&mut out, cols, true).unwrap();
        let rendered = String::from_utf8(out).unwrap();
        let line = rendered.strip_suffix("\r\n").unwrap();
        let stripped = line.trim_start_matches("\x1b[0m");
        assert_eq!(UnicodeWidthStr::width(stripped), cols as usize);
        assert!(FULL.starts_with(stripped.trim_end()), "{stripped:?}");
    }

    #[test]
    fn session_state_label_translates_when_chinese() {
        let attached = session("1", "a", true, false, 80, 24);
        let detached = session("2", "b", false, false, 80, 24);
        let exited = session("3", "c", false, true, 80, 24);
        assert_eq!(session_state_label(&attached, false), "attached");
        assert_eq!(session_state_label(&attached, true), "已连接");
        assert_eq!(session_state_label(&detached, true), "未连接");
        // The exit code is preserved inside the translated label.
        assert!(session_state_label(&exited, true).starts_with("已退出("));
    }

    #[test]
    fn pad_end_to_width_counts_display_columns() {
        assert_eq!(pad_end_to_width("ab", 4), "ab  ");
        // "已连接" is 6 display columns, so padding to 8 adds two spaces.
        assert_eq!(pad_end_to_width("已连接", 8), "已连接  ");
        // Never truncates when already at or beyond the target width.
        assert_eq!(pad_end_to_width("toolong", 3), "toolong");
    }

    #[test]
    fn session_row_aligns_wide_state_label_by_display_width() {
        // A row whose state contains wide CJK glyphs must pad the state column to
        // 14 display columns so later columns and the total line width stay aligned.
        let row = SessionListRow {
            session_id: "1".to_string(),
            name: "main".to_string(),
            state: "已连接".to_string(),
            size: "80x24".to_string(),
            cwd: "~".to_string(),
            cwd_raw: "~".to_string(),
            is_current: true,
            exited: false,
            attached: true,
        };
        let cols = 80u16;
        let mut out = Vec::new();
        write_session_row(&mut out, &row, false, cols).unwrap();
        let rendered = String::from_utf8(out).unwrap();
        let line = rendered.strip_suffix("\r\n").unwrap();
        let visible = line.trim_start_matches("\x1b[0m");
        assert_eq!(UnicodeWidthStr::width(visible), cols as usize);
        assert!(visible.contains("80x24"), "{visible:?}");
    }

    #[test]
    fn session_list_header_keeps_all_lines_when_they_fit() {
        assert_eq!(session_list_header_len(20, 5), 3);
    }

    #[test]
    fn session_list_header_drops_decorations_before_rows() {
        // budget 6, body 4: only the cosmetic blank separator is dropped.
        assert_eq!(session_list_header_len(6, 4), 2);
        // budget 5, body 4: the hint goes next.
        assert_eq!(session_list_header_len(5, 4), 1);
        // budget 3, body 10: everything drops so session rows survive.
        assert_eq!(session_list_header_len(3, 10), 0);
        assert_eq!(session_list_header_len(0, 1), 0);
    }

    #[test]
    fn session_list_viewport_keeps_selected_visible() {
        // Everything fits: the window is the whole list.
        assert_eq!(session_list_viewport(10, 4, 2), (0, 4));
        // Scrolled: the selected row sticks to the window's bottom edge.
        assert_eq!(session_list_viewport(3, 10, 0), (0, 3));
        assert_eq!(session_list_viewport(3, 10, 5), (3, 3));
        assert_eq!(session_list_viewport(3, 10, 9), (7, 3));
        // Degenerate budgets and out-of-range selections stay in bounds.
        assert_eq!(session_list_viewport(0, 10, 5), (0, 0));
        assert_eq!(session_list_viewport(3, 10, 42), (7, 3));
    }

    #[test]
    fn session_list_render_fits_short_terminal_and_shows_selected_row() {
        let rows: Vec<SessionListRow> = (0..10)
            .map(|i| SessionListRow {
                session_id: i.to_string(),
                name: format!("session{i}"),
                state: "running".to_string(),
                size: "80x24".to_string(),
                cwd: "~".to_string(),
                cwd_raw: "~".to_string(),
                is_current: i == 0,
                exited: false,
                attached: false,
            })
            .collect();
        let mut out = Vec::new();
        render_session_list(&mut out, &rows, 7, "", (80, 6), false).unwrap();
        let rendered = String::from_utf8(out).unwrap();
        // 6 terminal rows = 5 budget lines at most, so at most 5 CRLFs are
        // written (the status line carries none) and nothing scrolls.
        assert!(rendered.matches("\r\n").count() <= 5, "{rendered:?}");
        // The selected row is inside the viewport.
        assert!(rendered.contains("session7"), "{rendered:?}");
        // Rows far above the window are clipped out.
        assert!(!rendered.contains("session0"), "{rendered:?}");
    }

    #[test]
    fn session_list_hint_truncates_to_narrow_width() {
        // On a very narrow terminal, truncate by visible width without panicking or exceeding the width.
        let cols = 5u16;
        let mut out = Vec::new();
        write_session_list_hint(&mut out, cols, false).unwrap();
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

    fn now() -> Instant {
        Instant::now()
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
    fn prefix_key_event_matches_default_ctrl_b() {
        assert!(is_prefix_key_event(ctrl_key('b'), DEFAULT_PREFIX));
        assert!(is_prefix_key_event(ctrl_key('B'), DEFAULT_PREFIX));
        assert!(!is_prefix_key_event(ctrl_key('a'), DEFAULT_PREFIX));
    }

    #[test]
    fn prefix_key_event_matches_custom_ctrl_a() {
        let prefix = PrefixKey::parse("C-a").unwrap();
        assert!(is_prefix_key_event(ctrl_key('a'), prefix));
        assert!(is_prefix_key_event(ctrl_key('A'), prefix));
        assert!(!is_prefix_key_event(ctrl_key('b'), prefix));
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
                    term::InputModeState::default(),
                    now()
                ),
                vec![]
            );
            assert_eq!(
                state.handle_key(
                    char_key('d'),
                    prefix,
                    term::InputModeState::default(),
                    now()
                ),
                vec![AttachAction::Detach]
            );
        }
    }

    #[test]
    fn prefix_state_accepts_uppercase_actions() {
        let mut state = PrefixState::default();
        assert_eq!(
            state.handle_key(
                ctrl_key('b'),
                DEFAULT_PREFIX,
                term::InputModeState::default(),
                now()
            ),
            vec![]
        );
        assert_eq!(
            state.handle_key(
                char_key('D'),
                DEFAULT_PREFIX,
                term::InputModeState::default(),
                now()
            ),
            vec![AttachAction::Detach]
        );

        let mut state = PrefixState::default();
        assert_eq!(
            state.handle_key(
                ctrl_key('b'),
                DEFAULT_PREFIX,
                term::InputModeState::default(),
                now()
            ),
            vec![]
        );
        assert_eq!(
            state.handle_key(
                char_key('S'),
                DEFAULT_PREFIX,
                term::InputModeState::default(),
                now()
            ),
            vec![AttachAction::OpenSessionList]
        );
    }

    #[test]
    fn prefix_state_opens_help_for_question_mark() {
        for prefix in [DEFAULT_PREFIX, PrefixKey::parse("C-a").unwrap()] {
            let mut state = PrefixState::default();
            assert_eq!(
                state.handle_key(
                    ctrl_key(prefix.ctrl_char),
                    prefix,
                    term::InputModeState::default(),
                    now()
                ),
                vec![]
            );
            assert_eq!(
                state.handle_key(
                    char_key('?'),
                    prefix,
                    term::InputModeState::default(),
                    now()
                ),
                vec![AttachAction::OpenHelp]
            );
        }
    }

    #[test]
    fn session_list_key_mapping_covers_navigation_and_cancel() {
        assert_eq!(
            map_session_list_key(KeyCode::Up),
            Some(SessionListAction::MoveUp)
        );
        assert_eq!(
            map_session_list_key(KeyCode::Char('k')),
            Some(SessionListAction::MoveUp)
        );
        assert_eq!(
            map_session_list_key(KeyCode::Char('j')),
            Some(SessionListAction::MoveDown)
        );
        assert_eq!(
            map_session_list_key(KeyCode::Enter),
            Some(SessionListAction::Select)
        );
        assert_eq!(
            map_session_list_key(KeyCode::Char('c')),
            Some(SessionListAction::Create)
        );
        assert_eq!(
            map_session_list_key(KeyCode::Char('r')),
            Some(SessionListAction::Rename)
        );
        assert_eq!(
            map_session_list_key(KeyCode::Char('x')),
            Some(SessionListAction::Kill)
        );
        assert_eq!(
            map_session_list_key(KeyCode::Char('?')),
            Some(SessionListAction::Help)
        );
        assert_eq!(
            map_session_list_key(KeyCode::Esc),
            Some(SessionListAction::Cancel)
        );
        assert_eq!(
            map_session_list_key(KeyCode::Char('q')),
            Some(SessionListAction::Cancel)
        );
        assert_eq!(map_session_list_key(KeyCode::Char('z')), None);
    }

    #[test]
    fn name_edit_key_mapping_covers_edit_actions() {
        assert_eq!(map_name_edit_key(KeyCode::Enter), Some(NameEditKey::Submit));
        assert_eq!(map_name_edit_key(KeyCode::Esc), Some(NameEditKey::Cancel));
        assert_eq!(
            map_name_edit_key(KeyCode::Backspace),
            Some(NameEditKey::Backspace)
        );
        assert_eq!(
            map_name_edit_key(KeyCode::Char('a')),
            Some(NameEditKey::Char('a'))
        );
        assert_eq!(map_name_edit_key(KeyCode::Left), None);
    }

    #[test]
    fn prefix_help_rows_use_active_prefix_label() {
        let rows = prefix_help_rows(DEFAULT_PREFIX, false);
        // Every binding combo carries the active prefix label.
        assert!(rows.iter().all(|(combo, _)| combo.contains("Ctrl+B")));
        // The literal-prefix binding expands `<prefix>` to the label on both sides.
        assert!(
            rows.iter()
                .any(|(combo, _)| combo.contains("Ctrl+B Ctrl+B"))
        );
        // The help binding is present.
        assert!(rows.iter().any(|(combo, _)| combo.contains("Ctrl+B ?")));
        // A custom prefix relabels every combo.
        let rows_a = prefix_help_rows(PrefixKey::parse("C-a").unwrap(), false);
        assert!(rows_a.iter().all(|(combo, _)| combo.contains("Ctrl+A")));
    }

    #[test]
    fn prefix_state_passes_through_both_prefixes_for_double_prefix() {
        for prefix in [DEFAULT_PREFIX, PrefixKey::parse("C-b").unwrap()] {
            let mut state = PrefixState::default();
            assert_eq!(
                state.handle_key(
                    ctrl_key(prefix.ctrl_char),
                    prefix,
                    term::InputModeState::default(),
                    now()
                ),
                vec![]
            );
            // A second PREFIX is not a command key, so both bytes pass through.
            assert_eq!(
                state.handle_key(
                    ctrl_key(prefix.ctrl_char),
                    prefix,
                    term::InputModeState::default(),
                    now()
                ),
                vec![
                    AttachAction::Input(vec![prefix.byte]),
                    AttachAction::Input(vec![prefix.byte])
                ]
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
                    term::InputModeState::default(),
                    now()
                ),
                vec![]
            );
            assert_eq!(
                state.handle_key(
                    char_key('x'),
                    prefix,
                    term::InputModeState::default(),
                    now()
                ),
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
                term::InputModeState::default(),
                now()
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
                    term::InputModeState::default(),
                    now()
                ),
                vec![]
            );
            assert_eq!(
                state.handle_key(
                    char_key('s'),
                    prefix,
                    term::InputModeState::default(),
                    now()
                ),
                vec![AttachAction::OpenSessionList]
            );
        }
    }

    #[test]
    fn prefix_state_uses_raw_custom_prefix_bytes() {
        let prefix = PrefixKey::parse("C-b").unwrap();

        let mut state = PrefixState::default();
        assert_eq!(
            state.handle_key(
                raw_char_key(0x02),
                prefix,
                term::InputModeState::default(),
                now()
            ),
            vec![]
        );
        assert_eq!(
            state.handle_key(
                char_key('d'),
                prefix,
                term::InputModeState::default(),
                now()
            ),
            vec![AttachAction::Detach]
        );

        let mut state = PrefixState::default();
        assert_eq!(
            state.handle_key(
                raw_char_key(0x02),
                prefix,
                term::InputModeState::default(),
                now()
            ),
            vec![]
        );
        assert_eq!(
            state.handle_key(
                char_key('s'),
                prefix,
                term::InputModeState::default(),
                now()
            ),
            vec![AttachAction::OpenSessionList]
        );
    }

    #[test]
    fn prefix_state_passes_through_prefix_after_timeout() {
        let prefix = DEFAULT_PREFIX;
        let start = Instant::now();
        let mut state = PrefixState::default();
        assert_eq!(
            state.handle_key(
                ctrl_key(prefix.ctrl_char),
                prefix,
                term::InputModeState::default(),
                start
            ),
            vec![]
        );

        // Just under the timeout: nothing is flushed yet.
        assert_eq!(
            state.take_expired(
                start + PREFIX_PENDING_TIMEOUT - Duration::from_millis(1),
                prefix
            ),
            vec![]
        );

        // At the timeout the pending PREFIX byte is passed through.
        assert_eq!(
            state.take_expired(start + PREFIX_PENDING_TIMEOUT, prefix),
            vec![AttachAction::Input(vec![prefix.byte])]
        );

        // Once flushed, nothing remains pending.
        assert_eq!(
            state.take_expired(start + PREFIX_PENDING_TIMEOUT * 2, prefix),
            vec![]
        );
    }

    #[test]
    fn prefix_state_late_command_key_is_treated_as_normal_input() {
        let prefix = DEFAULT_PREFIX;
        let start = Instant::now();
        let mut state = PrefixState::default();
        assert_eq!(
            state.handle_key(
                ctrl_key(prefix.ctrl_char),
                prefix,
                term::InputModeState::default(),
                start
            ),
            vec![]
        );

        // A 'd' arriving after the timeout should not detach; the pending PREFIX
        // is flushed and 'd' is sent as ordinary input.
        assert_eq!(
            state.handle_key(
                char_key('d'),
                prefix,
                term::InputModeState::default(),
                start + PREFIX_PENDING_TIMEOUT
            ),
            vec![
                AttachAction::Input(vec![prefix.byte]),
                AttachAction::Input(vec![b'd'])
            ]
        );
    }

    #[test]
    fn session_list_rows_sort_like_list_output_and_mark_current() {
        let sessions = vec![
            session("3", "zeta", false, false, 100, 40),
            session("1", "alpha", true, false, 80, 24),
            session("2", "mid", false, true, 120, 30),
        ];
        let rows = build_session_list_rows(&sessions, "1", false);

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
        let rows = build_session_list_rows(&[info], "2", false);

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
            false,
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
            false,
        );
        let mut out = Vec::new();

        render_session_list(&mut out, &rows, 1, "ready", (80, 24), false).unwrap();

        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("qscreen sessions"));
        assert!(text.contains("  * 1    main"));
        assert!(text.contains(">   2    work"));
        assert!(text.contains("ready"));
    }

    #[test]
    fn render_session_list_uses_crlf_in_raw_mode() {
        let rows =
            build_session_list_rows(&[session("1", "main", true, false, 80, 24)], "1", false);
        let mut out = Vec::new();

        render_session_list(&mut out, &rows, 0, "", (80, 24), false).unwrap();

        for (idx, byte) in out.iter().enumerate() {
            if *byte == b'\n' {
                assert_eq!(idx.checked_sub(1).map(|prev| out[prev]), Some(b'\r'));
            }
        }
        assert!(out.windows(2).any(|window| window == b"\r\n"));
    }

    #[test]
    fn render_session_list_resets_style_and_pads_lines() {
        let rows =
            build_session_list_rows(&[session("1", "main", true, false, 80, 24)], "1", false);
        let mut out = Vec::new();

        render_session_list(&mut out, &rows, 0, "", (20, 8), false).unwrap();

        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("\x1b[?2026h\x1b[0m\x1b[2J"));
        assert!(text.contains("\x1b[0mqscreen sessions    \r\n"));
        assert!(text.contains("\x1b[0m> * 1    main"));
    }

    #[test]
    fn render_session_list_with_status_concatenates_segments() {
        // A three-segment status (rename label / input / hint) should render as
        // its concatenated text, matching the original single-line prompt.
        let rows =
            build_session_list_rows(&[session("1", "main", true, false, 80, 24)], "1", false);
        let mut out = Vec::new();

        let segments = [
            ("rename \"main\" -> ", color::sgr::PROMPT),
            ("work_", color::sgr::INPUT),
            ("  (Enter confirm, Esc cancel)", color::sgr::HINT),
        ];
        render_session_list_with_status(&mut out, &rows, 0, &segments, (80, 24), false).unwrap();

        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("rename \"main\" -> work_  (Enter confirm, Esc cancel)"));
    }

    #[test]
    fn write_status_segments_truncates_across_segments_and_pads() {
        // Segments together exceed the width: truncation spans segment
        // boundaries and the final line is padded to exactly `cols` columns.
        let mut out = Vec::new();
        let segments = [("abcdef", color::sgr::PROMPT), ("ghij", color::sgr::INPUT)];
        write_status_segments(&mut out, &segments, 8).unwrap();

        // Colorless test env: plain text truncated to 8 columns, no padding needed.
        assert_eq!(String::from_utf8(out).unwrap(), "abcdefgh");
    }

    #[test]
    fn write_status_segments_pads_short_line_to_width() {
        let mut out = Vec::new();
        let segments = [("hi", color::sgr::PROMPT)];
        write_status_segments(&mut out, &segments, 6).unwrap();

        assert_eq!(String::from_utf8(out).unwrap(), "hi    ");
    }

    fn session_info_with_id(session_id: &str, exited: bool) -> SessionInfo {
        SessionInfo {
            session_id: session_id.to_string(),
            exited,
            ..Default::default()
        }
    }

    #[test]
    fn highest_remaining_session_id_picks_largest_live_id() {
        let sessions = vec![
            session_info_with_id("2", false),
            session_info_with_id("10", false),
            session_info_with_id("3", false),
        ];
        assert_eq!(
            highest_remaining_session_id(&sessions, "3"),
            Some("10".to_string())
        );
    }

    #[test]
    fn highest_remaining_session_id_skips_ended_and_exited() {
        let sessions = vec![
            session_info_with_id("5", true),
            session_info_with_id("4", false),
            session_info_with_id("7", false),
        ];
        // The just-ended session (7) and the exited session (5) are ignored.
        assert_eq!(
            highest_remaining_session_id(&sessions, "7"),
            Some("4".to_string())
        );
    }

    #[test]
    fn highest_remaining_session_id_none_when_empty() {
        let sessions = vec![session_info_with_id("1", true)];
        assert_eq!(highest_remaining_session_id(&sessions, "1"), None);
    }

    #[test]
    fn highest_live_session_id_excluding_picks_largest_live_id() {
        let sessions = vec![
            session_info_with_id("2", false),
            session_info_with_id("10", false),
            session_info_with_id("3", false),
        ];
        assert_eq!(
            highest_live_session_id_excluding(&sessions, &[]),
            Some("10".to_string())
        );
    }

    #[test]
    fn highest_live_session_id_excluding_skips_excluded_and_exited() {
        let sessions = vec![
            session_info_with_id("9", true),
            session_info_with_id("8", false),
            session_info_with_id("6", false),
        ];
        // 9 is exited and 8 is excluded, so the next live candidate is 6.
        assert_eq!(
            highest_live_session_id_excluding(&sessions, &["8".to_string()]),
            Some("6".to_string())
        );
    }

    #[test]
    fn highest_live_session_id_excluding_none_when_all_skipped() {
        let sessions = vec![
            session_info_with_id("1", false),
            session_info_with_id("2", true),
        ];
        assert_eq!(
            highest_live_session_id_excluding(&sessions, &["1".to_string()]),
            None
        );
    }

    #[test]
    fn adjacent_session_id_moves_next_in_id_order() {
        let sessions = vec![
            session_info_with_id("2", false),
            session_info_with_id("10", false),
            session_info_with_id("3", false),
        ];
        // Sorted live IDs: 2, 3, 10. Next after 3 is 10.
        assert_eq!(
            adjacent_session_id(&sessions, "3", SwitchDirection::Next),
            Some("10".to_string())
        );
    }

    #[test]
    fn adjacent_session_id_moves_prev_in_id_order() {
        let sessions = vec![
            session_info_with_id("2", false),
            session_info_with_id("10", false),
            session_info_with_id("3", false),
        ];
        // Sorted live IDs: 2, 3, 10. Prev before 3 is 2.
        assert_eq!(
            adjacent_session_id(&sessions, "3", SwitchDirection::Prev),
            Some("2".to_string())
        );
    }

    #[test]
    fn adjacent_session_id_wraps_around_both_ends() {
        let sessions = vec![
            session_info_with_id("2", false),
            session_info_with_id("3", false),
            session_info_with_id("10", false),
        ];
        // Next past the highest ID wraps to the lowest.
        assert_eq!(
            adjacent_session_id(&sessions, "10", SwitchDirection::Next),
            Some("2".to_string())
        );
        // Prev before the lowest ID wraps to the highest.
        assert_eq!(
            adjacent_session_id(&sessions, "2", SwitchDirection::Prev),
            Some("10".to_string())
        );
    }

    #[test]
    fn adjacent_session_id_skips_exited_sessions() {
        let sessions = vec![
            session_info_with_id("2", false),
            session_info_with_id("3", true),
            session_info_with_id("10", false),
        ];
        // 3 is exited, so next after 2 is 10, not 3.
        assert_eq!(
            adjacent_session_id(&sessions, "2", SwitchDirection::Next),
            Some("10".to_string())
        );
    }

    #[test]
    fn adjacent_session_id_none_with_single_live_session() {
        let sessions = vec![
            session_info_with_id("2", false),
            session_info_with_id("3", true),
        ];
        assert_eq!(
            adjacent_session_id(&sessions, "2", SwitchDirection::Next),
            None
        );
        assert_eq!(
            adjacent_session_id(&sessions, "2", SwitchDirection::Prev),
            None
        );
    }

    #[test]
    fn prefix_state_switches_sessions_for_next_and_prev() {
        for prefix in [DEFAULT_PREFIX, PrefixKey::parse("C-b").unwrap()] {
            let mut state = PrefixState::default();
            assert_eq!(
                state.handle_key(
                    ctrl_key(prefix.ctrl_char),
                    prefix,
                    term::InputModeState::default(),
                    now()
                ),
                vec![]
            );
            assert_eq!(
                state.handle_key(
                    char_key('n'),
                    prefix,
                    term::InputModeState::default(),
                    now()
                ),
                vec![AttachAction::SwitchSession(SwitchDirection::Next)]
            );

            let mut state = PrefixState::default();
            assert_eq!(
                state.handle_key(
                    ctrl_key(prefix.ctrl_char),
                    prefix,
                    term::InputModeState::default(),
                    now()
                ),
                vec![]
            );
            assert_eq!(
                state.handle_key(
                    char_key('p'),
                    prefix,
                    term::InputModeState::default(),
                    now()
                ),
                vec![AttachAction::SwitchSession(SwitchDirection::Prev)]
            );
        }
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
