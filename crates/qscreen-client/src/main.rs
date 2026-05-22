use std::io::Write;
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::Context;
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use qscreen_protocol::{
    Command, EventType, Message, MessageKind, SessionInfo, validate_session_name,
};
use qscreen_shared::{daemon_log_path, pipe_name};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

mod term;

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
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    rt.block_on(async {
        match args.as_slice() {
            [] => cmd_default().await,
            [cmd] if cmd == "-h" || cmd == "--help" => {
                print_help();
                Ok(())
            }
            [cmd] if cmd == "ls" || cmd == "list" => cmd_list().await,
            [cmd] if cmd == "shutdown" => cmd_shutdown().await,
            [cmd, name] if cmd == "new" => cmd_new(name).await,
            [cmd, name] if cmd == "attach" || cmd == "-r" => cmd_attach(name).await,
            [cmd, name] if cmd == "kill" => cmd_kill(name).await,
            [cmd] if cmd == "new" => {
                let name = default_new_name();
                cmd_new_and_attach(&name).await
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
  qscn                         智能启动：无会话时新建并进入 main，单会话时直接 attach，
                            多会话时列出所有会话
  qscn new [<name>]            新建会话并进入（省略 name 时自动用时间戳命名）
  qscn attach <name>           进入已有会话
  qscn -r <name>               同 attach，兼容 tmux 风格
  qscn ls                      列出所有会话（同 list）
  qscn list                    列出所有会话
  qscn kill <name>             强制终止指定会话
  qscn shutdown                停止后台 daemon（所有会话将被关闭）
  qscn -h, --help              显示此帮助

会话内热键:
  Ctrl+A D                  从当前会话 detach（会话继续在后台运行）
  Ctrl+A Ctrl+A             向 PTY 发送字面 Ctrl+A 字符

ls 输出格式:
  <name>  <状态>  <创建时间>  <终端尺寸>
  状态: attached | detached | exited(<退出码>)

示例:
  qscn                         # 自动进入唯一会话，或新建 main
  qscn new work                # 新建名为 work 的会话
  qscn attach work             # 重新进入 work 会话
  qscn ls                      # 查看所有会话状态
  qscn kill work               # 终止 work 会话
"#
        );
    } else {
        println!(
            r#"qscreen — lightweight terminal session manager

Usage:
  qscn                         smart launch: create and enter 'main' if no sessions,
                            attach if one session, list all if multiple
  qscn new [<name>]            create a new session and attach (auto-name if omitted)
  qscn attach <name>           attach to an existing session
  qscn -r <name>               same as attach (tmux-style shorthand)
  qscn ls                      list all sessions (alias: list)
  qscn list                    list all sessions
  qscn kill <name>             forcibly terminate a session
  qscn shutdown                stop the background daemon (closes all sessions)
  qscn -h, --help              show this help

Key bindings (inside a session):
  Ctrl+A D                  detach from session (session keeps running)
  Ctrl+A Ctrl+A             send a literal Ctrl+A to the PTY

ls output format:
  <name>  <state>  <created-at>  <terminal-size>
  states: attached | detached | exited(<code>)

Examples:
  qscn                         # auto-attach or create main
  qscn new work                # create session named 'work'
  qscn attach work             # reattach to 'work'
  qscn ls                      # show all session states
  qscn kill work               # terminate 'work'
"#
        );
    }
}

// ── 子命令实现 ────────────────────────────────────────────────────────────────

async fn cmd_default() -> anyhow::Result<()> {
    let sessions = list_sessions().await?;
    match sessions.len() {
        0 => cmd_new_and_attach("main").await,
        1 => cmd_attach(&sessions[0].name.clone()).await,
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

async fn cmd_new(name: &str) -> anyhow::Result<()> {
    cmd_new_and_attach(name).await
}

async fn cmd_new_and_attach(name: &str) -> anyhow::Result<()> {
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
    attach_session(name).await
}

async fn cmd_attach(name: &str) -> anyhow::Result<()> {
    validate_session_name(name)?;
    attach_session(name).await
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
        let state = if s.exited {
            format!("exited({})", s.exit_code)
        } else if s.attached {
            "attached".to_string()
        } else {
            "detached".to_string()
        };
        let size = if s.size.is_empty() {
            format!("{}x{}", s.width, s.height)
        } else {
            s.size.clone()
        };
        let created = if s.created_at.timestamp() == 0 {
            "-".to_string()
        } else {
            s.created_at.format("%Y-%m-%dT%H:%M:%SZ").to_string()
        };
        println!("{}\t{}\t{}\t{}", s.name, state, created, size);
    }
}

// ── Attach 实现 ───────────────────────────────────────────────────────────────

async fn attach_session(name: &str) -> anyhow::Result<()> {
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

    crossterm::terminal::enable_raw_mode()?;

    #[cfg(windows)]
    {
        let _ = std::io::stdout().write_all(b"\x1b[?9001l");
        let _ = std::io::stdout().flush();
    }

    let name_owned = name.to_string();
    let result = run_attach_loop(conn, name_owned, term_size).await;

    // spawn_blocking 里 crossterm::event::read() 阻塞，无法通过 channel 取消
    // 直接 exit 让 OS 清理所有线程，父终端由 disable_raw_mode 恢复
    let code = match result {
        Ok(_) => 0i32,
        Err(_) => 1,
    };

    let _ = crossterm::terminal::disable_raw_mode();
    let _ = std::io::stdout().write_all(
        b"\x1b[?2026l\x1b[?2004l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1004l\x1b[?25h\x1b[0m\x1b[r",
    );
    #[cfg(windows)]
    let _ = std::io::stdout().write_all(b"\x1b[?9001l\x1b[!p");
    let _ = std::io::stdout().flush();

    std::process::exit(code);
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

async fn run_attach_loop(conn: TcpConn, name: String, term_size: (u16, u16)) -> anyhow::Result<()> {
    let (read_half, write_half) = tokio::io::split(conn.stream);
    let writer = std::sync::Arc::new(tokio::sync::Mutex::new(write_half));
    let mut reader = BufReader::new(read_half);

    let (cols, rows) = term_size;
    let mut screen = term::TermScreen::new(rows, cols);

    let writer_c = writer.clone();
    let name_c = name.clone();
    let mut msg_id: u64 = 10;

    let (stdin_tx, mut stdin_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    let (detach_tx, mut detach_rx) = tokio::sync::oneshot::channel::<()>();
    let (resize_tx, mut resize_rx) = tokio::sync::mpsc::unbounded_channel::<(u16, u16)>();

    let stdin_tx_bg = stdin_tx;
    let resize_tx_bg = resize_tx;
    let detach_tx = std::sync::Mutex::new(Some(detach_tx));

    // 键盘/resize 读取线程（crossterm::event::read() 是阻塞调用）
    tokio::task::spawn_blocking(move || {
        let mut pending_prefix = false;
        while let Ok(event) = crossterm::event::read() {
            match event {
                // 只处理按键按下事件，避免 key-up 重复输入
                Event::Key(key_event)
                    if matches!(key_event.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                {
                    // Ctrl+A 前缀检测（detach 热键：Ctrl+A D）
                    let is_ctrl_a = key_event.modifiers.contains(KeyModifiers::CONTROL)
                        && key_event.code == KeyCode::Char('a');

                    if is_ctrl_a {
                        if pending_prefix {
                            // 双 Ctrl+A → 发送一个字面 Ctrl+A 到 PTY
                            pending_prefix = false;
                            let _ = stdin_tx_bg.send(vec![0x01]);
                        } else {
                            pending_prefix = true;
                        }
                        continue;
                    }

                    if pending_prefix {
                        pending_prefix = false;
                        if key_event.code == KeyCode::Char('d') {
                            // Ctrl+A D → detach
                            if let Some(tx) = detach_tx.lock().unwrap().take() {
                                let _ = tx.send(());
                            }
                            return;
                        }
                        // 非 D → 先补发 Ctrl+A，再处理当前键
                        let _ = stdin_tx_bg.send(vec![0x01]);
                    }

                    let bytes = key_event_to_bytes(key_event);
                    if !bytes.is_empty() {
                        let _ = stdin_tx_bg.send(bytes);
                    }
                }

                Event::Resize(w, h) => {
                    let _ = resize_tx_bg.send((w, h));
                }

                _ => {}
            }
        }
    });

    let mut stdout = std::io::stdout();
    let mut line = String::new();

    loop {
        line.clear();
        tokio::select! {
            result = reader.read_line(&mut line) => {
                match result {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let msg = match Message::from_json(&line) {
                    Ok(m) => m,
                    Err(_) => break,
                };
                match msg.kind {
                    MessageKind::Event => match msg.event {
                        Some(EventType::Output) => {
                            screen.process(&msg.payload);
                            let _ = screen.render(&mut stdout);
                        }
                        Some(EventType::Exit) => break,
                        _ => {}
                    },
                    MessageKind::Response if !msg.error.is_empty() => {
                        break;
                    }
                    _ => {}
                }
            }

            bytes = stdin_rx.recv() => {
                match bytes {
                    None => break,
                    Some(data) => {
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
                            break;
                        }
                    }
                }
            }

            size = resize_rx.recv() => {
                if let Some((w, h)) = size {
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
                        break;
                    }
                }
            }

            _ = &mut detach_rx => {
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
                break;
            }
        }
    }

    Ok(())
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
