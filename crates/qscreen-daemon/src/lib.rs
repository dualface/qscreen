pub mod session;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::Context;
use qscreen_protocol::{
    attached_session_error, duplicate_session_error, exited_session_error, missing_session_error,
    validate_new_size, validate_resize, validate_session_name, Command, EventType, Message,
    MessageKind, SessionInfo, MAX_PAYLOAD_SIZE,
};
use qscreen_shared::pipe_name;
use session::{Session, SessionEvent};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, watch};

// ── Daemon 共享状态 ───────────────────────────────────────────────────────────

struct State {
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    stop_tx: watch::Sender<bool>,
}

impl State {
    fn new(stop_tx: watch::Sender<bool>) -> Self {
        State {
            sessions: Mutex::new(HashMap::new()),
            stop_tx,
        }
    }

    fn get_session(&self, name: &str) -> Option<Arc<Session>> {
        self.sessions.lock().unwrap().get(name).cloned()
    }

    fn list_sessions(&self) -> Vec<SessionInfo> {
        let guard = self.sessions.lock().unwrap();
        let mut infos: Vec<SessionInfo> = guard
            .values()
            .map(|s| session_info(s))
            .collect();
        infos.sort_by(|a, b| a.name.cmp(&b.name));
        infos
    }

    fn stop(&self) {
        let _ = self.stop_tx.send(true);
    }

    /// 关闭并移除所有 session
    fn kill_all(&self) {
        let sessions: Vec<Arc<Session>> = {
            let mut guard = self.sessions.lock().unwrap();
            let vals: Vec<_> = guard.values().cloned().collect();
            guard.clear();
            vals
        };
        for s in sessions {
            s.close();
        }
    }
}

fn session_info(s: &Session) -> SessionInfo {
    let w = s.width() as u32;
    let h = s.height() as u32;
    SessionInfo {
        name: s.name.clone(),
        attached: s.is_attached(),
        exited: s.exited.load(std::sync::atomic::Ordering::SeqCst),
        exit_code: s
            .exit_code
            .lock()
            .unwrap()
            .unwrap_or(0) as i64,
        created_at: s.created_at,
        width: w,
        height: h,
        size: format!("{}x{}", w, h),
    }
}

// ── 入口：daemon 主循环 ───────────────────────────────────────────────────────

/// 在当前线程运行 daemon（需要在 tokio runtime 内调用）
pub async fn run() -> anyhow::Result<()> {
    let pipe = pipe_name();
    tracing::info!(pipe = %pipe, "daemon starting");

    let (stop_tx, mut stop_rx) = watch::channel(false);
    let state = Arc::new(State::new(stop_tx));

    #[cfg(windows)]
    {
        use tokio::net::windows::named_pipe::ServerOptions;

        let mut server = ServerOptions::new()
            .first_pipe_instance(true)
            .create(&pipe)
            .context("create named pipe")?;

        loop {
            tokio::select! {
                result = server.connect() => {
                    if let Err(e) = result {
                        tracing::warn!("pipe connect error: {}", e);
                        continue;
                    }

                    // 立即准备下一个 pipe instance，再把当前连接交给 handler
                    let next = match ServerOptions::new().create(&pipe) {
                        Ok(s) => s,
                        Err(e) => {
                            tracing::error!("create next pipe instance failed: {}", e);
                            break;
                        }
                    };
                    let conn = std::mem::replace(&mut server, next);
                    let state_c = state.clone();
                    tokio::spawn(async move {
                        handle_connection(conn, state_c).await;
                    });
                }
                _ = stop_rx.changed() => {
                    if *stop_rx.borrow() {
                        tracing::info!("daemon stop signal received");
                        break;
                    }
                }
            }
        }
    }

    #[cfg(not(windows))]
    {
        anyhow::bail!("qscreen-daemon 仅支持 Windows (ConPTY)");
    }

    state.kill_all();
    tracing::info!("daemon stopped");
    Ok(())
}

// ── 连接处理 ─────────────────────────────────────────────────────────────────

#[cfg(windows)]
async fn handle_connection(
    stream: tokio::net::windows::named_pipe::NamedPipeServer,
    state: Arc<State>,
) {
    let (read_half, write_half) = tokio::io::split(stream);
    let writer: SharedWriter = Arc::new(tokio::sync::Mutex::new(write_half));
    let mut reader = BufReader::new(read_half);

    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => {
                tracing::debug!("client read error: {}", e);
                break;
            }
        }

        let msg = match Message::from_json(&line) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!("protocol parse error: {}", e);
                break;
            }
        };

        if msg.kind != MessageKind::Request {
            break;
        }
        if msg.id.is_empty() {
            break;
        }

        // attach 命令把控制权转移到 handle_attach
        if msg.command == Some(Command::Attach) {
            handle_attach(msg, reader, writer, state).await;
            return;
        }

        let resp = dispatch_command(&msg, &state).await;
        let bytes = match resp.to_json_line() {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!("encode response error: {}", e);
                break;
            }
        };
        if writer.lock().await.write_all(&bytes).await.is_err() {
            break;
        }

        // stop 命令在回复后触发 daemon 停止
        if msg.command == Some(Command::Stop) {
            break;
        }
    }
}

type SharedWriter = Arc<
    tokio::sync::Mutex<
        tokio::io::WriteHalf<tokio::net::windows::named_pipe::NamedPipeServer>,
    >,
>;

/// 非 attach 命令的统一 dispatch
async fn dispatch_command(msg: &Message, state: &State) -> Message {
    let id = msg.id.clone();
    let result = dispatch_inner(msg, state).await;
    match result {
        Ok(mut resp) => {
            resp.id = id;
            resp
        }
        Err(e) => Message {
            kind: MessageKind::Response,
            id,
            error: e.to_string(),
            ..Default::default()
        },
    }
}

async fn dispatch_inner(msg: &Message, state: &State) -> anyhow::Result<Message> {
    match &msg.command {
        Some(Command::New) => {
            validate_session_name(&msg.name)?;
            validate_new_size(msg.width, msg.height)?;
            {
                let guard = state.sessions.lock().unwrap();
                if let Some(existing) = guard.get(&msg.name) {
                    if !existing.exited.load(std::sync::atomic::Ordering::SeqCst) {
                        anyhow::bail!("{}", duplicate_session_error(&msg.name));
                    }
                }
            }
            let sess = Session::new(msg.name.clone(), msg.width, msg.height)?;
            tracing::info!(session = %msg.name, "session created");
            state.sessions.lock().unwrap().insert(msg.name.clone(), sess);
            Ok(Message {
                kind: MessageKind::Response,
                ok: true,
                ..Default::default()
            })
        }

        Some(Command::List) => Ok(Message {
            kind: MessageKind::Response,
            ok: true,
            sessions: state.list_sessions(),
            ..Default::default()
        }),

        Some(Command::Kill) => {
            validate_session_name(&msg.name)?;
            let sess = state
                .get_session(&msg.name)
                .ok_or_else(|| anyhow::anyhow!("{}", missing_session_error(&msg.name)))?;
            sess.close();
            state.sessions.lock().unwrap().remove(&msg.name);
            tracing::info!(session = %msg.name, "session killed");
            Ok(Message {
                kind: MessageKind::Response,
                ok: true,
                ..Default::default()
            })
        }

        Some(Command::Stop) => {
            state.kill_all();
            state.stop();
            tracing::info!("daemon stop requested");
            Ok(Message {
                kind: MessageKind::Response,
                ok: true,
                ..Default::default()
            })
        }

        Some(Command::Input) => {
            validate_session_name(&msg.name)?;
            let sess = state
                .get_session(&msg.name)
                .ok_or_else(|| anyhow::anyhow!("{}", missing_session_error(&msg.name)))?;
            if !msg.payload.is_empty() {
                sess.write_input(&msg.payload)
                    .map_err(|e| session_error(&msg.name, e))?;
            }
            Ok(Message {
                kind: MessageKind::Response,
                ok: true,
                ..Default::default()
            })
        }

        Some(Command::Resize) => {
            validate_session_name(&msg.name)?;
            validate_resize(msg.width, msg.height)?;
            let sess = state
                .get_session(&msg.name)
                .ok_or_else(|| anyhow::anyhow!("{}", missing_session_error(&msg.name)))?;
            sess.resize(msg.width, msg.height)
                .map_err(|e| session_error(&msg.name, e))?;
            Ok(Message {
                kind: MessageKind::Response,
                ok: true,
                ..Default::default()
            })
        }

        _ => anyhow::bail!("unknown or missing command"),
    }
}

/// attach 命令处理：握手 → 发送 scrollback → 双向 IO 循环
#[cfg(windows)]
async fn handle_attach(
    msg: Message,
    mut reader: BufReader<
        tokio::io::ReadHalf<tokio::net::windows::named_pipe::NamedPipeServer>,
    >,
    writer: SharedWriter,
    state: Arc<State>,
) {
    let session_name = msg.name.clone();
    let attach_id = msg.id.clone();

    // 校验名称
    if let Err(e) = validate_session_name(&session_name) {
        let resp = Message {
            kind: MessageKind::Response,
            id: attach_id,
            error: e.to_string(),
            ..Default::default()
        };
        let _ = writer
            .lock()
            .await
            .write_all(&resp.to_json_line().unwrap_or_default())
            .await;
        return;
    }

    // 获取 session
    let sess = match state.get_session(&session_name) {
        Some(s) => s,
        None => {
            let resp = Message {
                kind: MessageKind::Response,
                id: attach_id,
                error: missing_session_error(&session_name),
                ..Default::default()
            };
            let _ = writer
                .lock()
                .await
                .write_all(&resp.to_json_line().unwrap_or_default())
                .await;
            return;
        }
    };

    // 注册事件接收 channel
    let (tx, mut rx) = mpsc::unbounded_channel::<SessionEvent>();
    let scrollback = match sess.attach(tx) {
        Ok(sb) => sb,
        Err(e) => {
            let resp = Message {
                kind: MessageKind::Response,
                id: attach_id,
                error: session_error(&session_name, e).to_string(),
                ..Default::default()
            };
            let _ = writer
                .lock()
                .await
                .write_all(&resp.to_json_line().unwrap_or_default())
                .await;
            return;
        }
    };

    tracing::info!(session = %session_name, "client attached");

    // 发送 attach 成功响应
    let ok_resp = Message {
        kind: MessageKind::Response,
        id: attach_id,
        ok: true,
        ..Default::default()
    };
    if writer
        .lock()
        .await
        .write_all(&ok_resp.to_json_line().unwrap_or_default())
        .await
        .is_err()
    {
        sess.detach();
        return;
    }

    // 发送 scrollback（分块，每块不超过 MAX_PAYLOAD_SIZE）
    let mut offset = 0;
    while offset < scrollback.len() {
        let end = (offset + MAX_PAYLOAD_SIZE).min(scrollback.len());
        let chunk = scrollback[offset..end].to_vec();
        offset = end;
        let event = Message {
            kind: MessageKind::Event,
            event: Some(EventType::Output),
            payload: chunk,
            ..Default::default()
        };
        if writer
            .lock()
            .await
            .write_all(&event.to_json_line().unwrap_or_default())
            .await
            .is_err()
        {
            sess.detach();
            return;
        }
    }

    // 启动 writer 任务：PTY 输出 → client
    let writer_task_handle = {
        let writer_c = writer.clone();
        let name_c = session_name.clone();
        tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                let msg = match event {
                    SessionEvent::Output(data) => Message {
                        kind: MessageKind::Event,
                        event: Some(EventType::Output),
                        payload: data.to_vec(),
                        ..Default::default()
                    },
                    SessionEvent::Exit(code) => Message {
                        kind: MessageKind::Event,
                        event: Some(EventType::Exit),
                        exit_code: code as i64,
                        ..Default::default()
                    },
                };
                let bytes = msg.to_json_line().unwrap_or_default();
                if writer_c.lock().await.write_all(&bytes).await.is_err() {
                    break;
                }
                // exit event 后退出 writer 任务
                if msg.event == Some(EventType::Exit) {
                    break;
                }
            }
            tracing::debug!(session = %name_c, "writer task ended");
        })
    };

    // Reader 循环：client → PTY（Input / Resize / Detach）
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => break,
        }

        let cmd = match Message::from_json(&line) {
            Ok(m) => m,
            Err(e) => {
                tracing::debug!(session = %session_name, "parse error: {}", e);
                break;
            }
        };

        match &cmd.command {
            Some(Command::Input) => {
                if !cmd.payload.is_empty() {
                    if let Err(e) = sess.write_input(&cmd.payload) {
                        tracing::debug!(session = %session_name, "write input error: {}", e);
                    }
                }
                // 兼容 Go 协议：input 也发 ok 响应
                let resp = Message {
                    kind: MessageKind::Response,
                    id: cmd.id.clone(),
                    ok: true,
                    ..Default::default()
                };
                if writer
                    .lock()
                    .await
                    .write_all(&resp.to_json_line().unwrap_or_default())
                    .await
                    .is_err()
                {
                    break;
                }
            }
            Some(Command::Resize) => {
                if cmd.width > 0 && cmd.height > 0 {
                    let _ = sess.resize(cmd.width, cmd.height);
                }
                let resp = Message {
                    kind: MessageKind::Response,
                    id: cmd.id.clone(),
                    ok: true,
                    ..Default::default()
                };
                if writer
                    .lock()
                    .await
                    .write_all(&resp.to_json_line().unwrap_or_default())
                    .await
                    .is_err()
                {
                    break;
                }
            }
            Some(Command::Detach) => {
                sess.detach();
                let resp = Message {
                    kind: MessageKind::Response,
                    id: cmd.id.clone(),
                    ok: true,
                    ..Default::default()
                };
                let _ = writer
                    .lock()
                    .await
                    .write_all(&resp.to_json_line().unwrap_or_default())
                    .await;
                tracing::info!(session = %session_name, "client detached");
                writer_task_handle.abort();
                return;
            }
            _ => break,
        }
    }

    sess.detach();
    writer_task_handle.abort();
    tracing::debug!(session = %session_name, "client disconnected");
}

fn session_error(name: &str, e: anyhow::Error) -> anyhow::Error {
    let msg = e.to_string();
    if msg.contains("already attached") {
        anyhow::anyhow!("{}", attached_session_error(name))
    } else if msg.contains("exited") || msg.contains("closed") {
        anyhow::anyhow!("{}", exited_session_error(name))
    } else {
        e
    }
}
