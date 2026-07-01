pub mod session;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::Context;
use qscreen_protocol::{
    AttachMode, Command, EventType, Message, MessageKind, SessionInfo, attached_session_error,
    exited_session_error, missing_session_error, validate_attach_size, validate_new_size,
    validate_resize, validate_session_id, validate_session_name,
};
#[cfg(unix)]
use qscreen_shared::daemon_lock_path;
use qscreen_shared::pipe_name;
use session::{Session, SessionEvent, SessionEventQueue};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{oneshot, watch};

// ── Daemon 共享状态 ───────────────────────────────────────────────────────────

struct State {
    sessions: Arc<Mutex<HashMap<String, Arc<Session>>>>,
    exited_sessions: Arc<Mutex<HashMap<String, SessionInfo>>>,
    next_session_id: Mutex<u64>,
    stop_tx: watch::Sender<bool>,
}

impl State {
    fn new(stop_tx: watch::Sender<bool>) -> Self {
        State {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            exited_sessions: Arc::new(Mutex::new(HashMap::new())),
            next_session_id: Mutex::new(1),
            stop_tx,
        }
    }

    fn get_session(&self, session_id: &str) -> Option<Arc<Session>> {
        self.sessions.lock().unwrap().get(session_id).cloned()
    }

    fn get_exited_session(&self, session_id: &str) -> Option<SessionInfo> {
        self.exited_sessions
            .lock()
            .unwrap()
            .get(session_id)
            .cloned()
    }

    fn insert_session(&self, session_id: String, session: Arc<Session>) {
        self.exited_sessions.lock().unwrap().remove(&session_id);
        self.sessions
            .lock()
            .unwrap()
            .insert(session_id.clone(), session.clone());
        let mut exit_rx = session.subscribe_exit();
        let sessions = self.sessions.clone();
        let exited_sessions = self.exited_sessions.clone();
        tokio::spawn(async move {
            if !*exit_rx.borrow() {
                let _ = exit_rx.changed().await;
            }
            let info = session_info(&session);
            let removed = {
                let mut guard = sessions.lock().unwrap();
                match guard.get(&session_id) {
                    Some(current) if Arc::ptr_eq(current, &session) => {
                        guard.remove(&session_id).is_some()
                    }
                    _ => false,
                }
            };
            if removed {
                let name = session.name();
                exited_sessions
                    .lock()
                    .unwrap()
                    .insert(session_id.clone(), info);
                tracing::info!(session_id = %session_id, session = %name, "session cleaned up");
            }
        });
    }

    fn allocate_session_id(&self) -> String {
        let mut next = self.next_session_id.lock().unwrap();
        loop {
            let session_id = next.to_string();
            *next = next.saturating_add(1);
            if !self.sessions.lock().unwrap().contains_key(&session_id) {
                return session_id;
            }
        }
    }

    fn list_sessions(&self) -> Vec<SessionInfo> {
        let guard = self.sessions.lock().unwrap();
        let mut infos: Vec<SessionInfo> = guard.values().map(|s| session_info(s)).collect();
        drop(guard);
        infos.extend(self.exited_sessions.lock().unwrap().values().cloned());
        infos.sort_by_key(|info| info.session_id.parse::<u64>().unwrap_or(u64::MAX));
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
        self.exited_sessions.lock().unwrap().clear();
    }

    fn remove_session(&self, session_id: &str) {
        self.sessions.lock().unwrap().remove(session_id);
        self.exited_sessions.lock().unwrap().remove(session_id);
    }
}

fn session_info(s: &Session) -> SessionInfo {
    let w = s.width() as u32;
    let h = s.height() as u32;
    SessionInfo {
        session_id: s.session_id.clone(),
        name: s.name(),
        attached: s.is_attached(),
        exited: s.exited.load(std::sync::atomic::Ordering::SeqCst),
        exit_code: s.exit_code.lock().unwrap().unwrap_or(0) as i64,
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

    #[cfg(unix)]
    {
        let _lock_guard = DaemonLockGuard::acquire()?;
        let _ = std::fs::remove_file(&pipe);
        let listener = tokio::net::UnixListener::bind(&pipe)
            .with_context(|| format!("bind unix socket {}", pipe))?;

        loop {
            tokio::select! {
                result = listener.accept() => {
                    match result {
                        Ok((conn, _)) => {
                            let state_c = state.clone();
                            tokio::spawn(async move {
                                handle_connection(conn, state_c).await;
                            });
                        }
                        Err(e) => tracing::warn!("unix socket accept error: {}", e),
                    }
                }
                _ = stop_rx.changed() => {
                    if *stop_rx.borrow() {
                        tracing::info!("daemon stop signal received");
                        break;
                    }
                }
            }
        }
        let _ = std::fs::remove_file(&pipe);
    }

    state.kill_all();
    tracing::info!("daemon stopped");
    Ok(())
}

#[cfg(unix)]
struct DaemonLockGuard {
    path: std::path::PathBuf,
}

#[cfg(unix)]
impl DaemonLockGuard {
    fn acquire() -> anyhow::Result<Self> {
        let path = daemon_lock_path();
        match std::fs::create_dir(&path) {
            Ok(()) => {
                let pid_path = path.join("pid");
                let _ = std::fs::write(&pid_path, std::process::id().to_string());
                Ok(Self { path })
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let socket = pipe_name();
                let stale = !std::path::Path::new(&socket).exists()
                    || std::os::unix::net::UnixStream::connect(&socket).is_err();
                if stale {
                    let _ = std::fs::remove_dir_all(&path);
                    std::fs::create_dir(&path)
                        .with_context(|| format!("create daemon lock {}", path.display()))?;
                    let _ = std::fs::write(path.join("pid"), std::process::id().to_string());
                    return Ok(Self { path });
                }
                anyhow::bail!("daemon already running")
            }
            Err(e) => Err(e).with_context(|| format!("create daemon lock {}", path.display())),
        }
    }
}

#[cfg(unix)]
impl Drop for DaemonLockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir(&self.path);
    }
}

// ── 连接处理 ─────────────────────────────────────────────────────────────────

async fn handle_connection<S>(stream: S, state: Arc<State>)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (read_half, write_half) = tokio::io::split(stream);
    let writer = Arc::new(tokio::sync::Mutex::new(write_half));
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

type SharedWriter<W> = Arc<tokio::sync::Mutex<W>>;

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
            session_id: msg.session_id.clone(),
            error: e.to_string(),
            ..Default::default()
        },
    }
}

async fn dispatch_inner(msg: &Message, state: &State) -> anyhow::Result<Message> {
    match &msg.command {
        Some(Command::New) => {
            validate_new_size(msg.width, msg.height)?;
            if !msg.name.is_empty() {
                validate_session_name(&msg.name)?;
            }
            let session_id = state.allocate_session_id();
            let session_name = if msg.name.is_empty() {
                session_id.clone()
            } else {
                msg.name.clone()
            };
            let sess = Session::new(
                session_id.clone(),
                session_name.clone(),
                msg.width,
                msg.height,
                Some(msg.shell.as_str()),
            )?;
            tracing::info!(session_id = %session_id, session = %session_name, "session created");
            state.insert_session(session_id.clone(), sess);
            Ok(Message {
                kind: MessageKind::Response,
                session_id,
                name: session_name,
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
            validate_session_id(&msg.session_id)?;
            if let Some(sess) = state.get_session(&msg.session_id) {
                let session_name = sess.name();
                sess.close();
                state.remove_session(&msg.session_id);
                tracing::info!(session_id = %msg.session_id, session = %session_name, "session killed");
            } else if state.get_exited_session(&msg.session_id).is_some() {
                state.remove_session(&msg.session_id);
                tracing::info!(session_id = %msg.session_id, "exited session removed");
            } else {
                anyhow::bail!("{}", missing_session_error(&msg.session_id));
            }
            Ok(Message {
                kind: MessageKind::Response,
                session_id: msg.session_id.clone(),
                ok: true,
                ..Default::default()
            })
        }

        Some(Command::Rename) => {
            validate_session_id(&msg.session_id)?;
            validate_session_name(&msg.name)?;
            if let Some(sess) = state.get_session(&msg.session_id) {
                sess.rename(msg.name.clone());
            } else if let Some(mut info) = state.get_exited_session(&msg.session_id) {
                info.name = msg.name.clone();
                state
                    .exited_sessions
                    .lock()
                    .unwrap()
                    .insert(msg.session_id.clone(), info);
            } else {
                anyhow::bail!("{}", missing_session_error(&msg.session_id));
            }
            tracing::info!(session_id = %msg.session_id, session = %msg.name, "session renamed");
            Ok(Message {
                kind: MessageKind::Response,
                session_id: msg.session_id.clone(),
                name: msg.name.clone(),
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
                session_id: msg.session_id.clone(),
                ok: true,
                ..Default::default()
            })
        }

        Some(Command::Input) => {
            validate_session_id(&msg.session_id)?;
            let sess = state
                .get_session(&msg.session_id)
                .ok_or_else(|| anyhow::anyhow!("{}", missing_session_error(&msg.session_id)))?;
            if !msg.payload.is_empty() {
                sess.write_input(&msg.payload)
                    .map_err(|e| session_error(&msg.session_id, e))?;
            }
            Ok(Message {
                kind: MessageKind::Response,
                session_id: msg.session_id.clone(),
                ok: true,
                ..Default::default()
            })
        }

        Some(Command::Resize) => {
            validate_session_id(&msg.session_id)?;
            validate_resize(msg.width, msg.height)?;
            let sess = state
                .get_session(&msg.session_id)
                .ok_or_else(|| anyhow::anyhow!("{}", missing_session_error(&msg.session_id)))?;
            sess.resize(msg.width, msg.height)
                .map_err(|e| session_error(&msg.session_id, e))?;
            Ok(Message {
                kind: MessageKind::Response,
                ok: true,
                ..Default::default()
            })
        }

        _ => anyhow::bail!("unknown or missing command"),
    }
}

/// attach 命令处理：握手 → 发送当前 screen frame → 双向 IO 循环
async fn handle_attach<R, W>(
    msg: Message,
    mut reader: BufReader<R>,
    writer: SharedWriter<W>,
    state: Arc<State>,
) where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let session_id = msg.session_id.clone();
    let attach_id = msg.id.clone();

    // 校验 session_id
    if let Err(e) = validate_session_id(&session_id) {
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

    if let Err(e) = validate_attach_size(msg.width, msg.height) {
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
    let sess = match state.get_session(&session_id) {
        Some(s) => s,
        None => {
            let error = if state.get_exited_session(&session_id).is_some() {
                exited_session_error(&session_id)
            } else {
                missing_session_error(&session_id)
            };
            let resp = Message {
                kind: MessageKind::Response,
                id: attach_id,
                error,
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

    // 注册事件接收 queue
    let event_queue = SessionEventQueue::new();
    let attach_mode = msg.attach_mode;
    let (client_id, initial_event) =
        match sess.attach(event_queue.clone(), msg.width, msg.height, attach_mode) {
            Ok(result) => result,
            Err(e) => {
                let resp = Message {
                    kind: MessageKind::Response,
                    id: attach_id,
                    error: session_error(&session_id, e).to_string(),
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

    let attached_name = sess.name();
    tracing::info!(session_id = %session_id, session = %attached_name, client_id, "client attached");

    // 发送 attach 成功响应
    let ok_resp = Message {
        kind: MessageKind::Response,
        id: attach_id,
        session_id: session_id.clone(),
        name: attached_name,
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
        sess.disconnect(client_id);
        return;
    }

    if attach_mode == AttachMode::Frame {
        let snapshot = sess.scrollback_snapshot();
        if !snapshot.is_empty() {
            for replay_msg in event_messages(&session_id, SessionEvent::Output(snapshot)) {
                let bytes = match replay_msg.to_json_line() {
                    Ok(bytes) => bytes,
                    Err(e) => {
                        tracing::warn!(session_id = %session_id, error = %e, "serialize attach replay event failed");
                        sess.disconnect(client_id);
                        return;
                    }
                };
                if writer.lock().await.write_all(&bytes).await.is_err() {
                    sess.disconnect(client_id);
                    return;
                }
            }
        }
    }

    if let Some(event) = initial_event {
        for initial_msg in event_messages(&session_id, event) {
            let initial_bytes = match initial_msg.to_json_line() {
                Ok(bytes) => bytes,
                Err(e) => {
                    tracing::warn!(session_id = %session_id, error = %e, "serialize attach initial event failed");
                    sess.disconnect(client_id);
                    return;
                }
            };
            if writer.lock().await.write_all(&initial_bytes).await.is_err() {
                sess.disconnect(client_id);
                return;
            }
        }
    }

    // 启动 writer 任务：PTY 输出 → client
    let (writer_done_tx, mut writer_done_rx) = oneshot::channel::<()>();
    let writer_task_handle = {
        let writer_c = writer.clone();
        let session_id_c = session_id.clone();
        let name_c = sess.name();
        tokio::spawn(async move {
            loop {
                let Some(event) = event_queue.recv().await else {
                    break;
                };
                let is_exit = matches!(event, SessionEvent::Exit(_));
                for msg in event_messages(&session_id_c, event) {
                    let bytes = match msg.to_json_line() {
                        Ok(bytes) => bytes,
                        Err(e) => {
                            tracing::warn!(session_id = %session_id_c, error = %e, "serialize attach event failed");
                            return;
                        }
                    };
                    if writer_c.lock().await.write_all(&bytes).await.is_err() {
                        return;
                    }
                }
                if is_exit {
                    break;
                }
            }
            tracing::debug!(session_id = %session_id_c, session = %name_c, "writer task ended");
            let _ = writer_done_tx.send(());
        })
    };

    // Reader 循环：client → PTY（Input / Resize / Detach）
    let mut line = String::new();
    loop {
        line.clear();
        let read_result = tokio::select! {
            result = reader.read_line(&mut line) => result,
            _ = &mut writer_done_rx => break,
        };
        match read_result {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => break,
        }

        let cmd = match Message::from_json(&line) {
            Ok(m) => m,
            Err(e) => {
                tracing::debug!(session_id = %session_id, "parse error: {}", e);
                break;
            }
        };

        if cmd.session_id != session_id {
            tracing::debug!(
                session_id = %session_id,
                got_session_id = %cmd.session_id,
                "attach stream command had mismatched session_id"
            );
            break;
        }

        match &cmd.command {
            Some(Command::Focus) => {
                if let Err(e) = sess.focus_client(client_id) {
                    let session_name = sess.name();
                    tracing::debug!(session_id = %session_id, session = %session_name, client_id, "focus error: {}", e);
                }
                let resp = Message {
                    kind: MessageKind::Response,
                    id: cmd.id.clone(),
                    session_id: session_id.clone(),
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
            Some(Command::Input) => {
                if let Err(e) = sess.input_client(client_id, &cmd.payload) {
                    let session_name = sess.name();
                    tracing::debug!(session_id = %session_id, session = %session_name, "write input error: {}", e);
                }
                // 兼容 Go 协议：input 也发 ok 响应
                let resp = Message {
                    kind: MessageKind::Response,
                    id: cmd.id.clone(),
                    session_id: session_id.clone(),
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
                let resp = match validate_resize(cmd.width, cmd.height)
                    .and_then(|_| sess.resize_client(client_id, cmd.width, cmd.height))
                {
                    Ok(()) => Message {
                        kind: MessageKind::Response,
                        id: cmd.id.clone(),
                        session_id: session_id.clone(),
                        ok: true,
                        ..Default::default()
                    },
                    Err(e) => Message {
                        kind: MessageKind::Response,
                        id: cmd.id.clone(),
                        session_id: session_id.clone(),
                        error: e.to_string(),
                        ..Default::default()
                    },
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
                sess.detach(client_id);
                let resp = Message {
                    kind: MessageKind::Response,
                    id: cmd.id.clone(),
                    session_id: session_id.clone(),
                    ok: true,
                    ..Default::default()
                };
                let _ = writer
                    .lock()
                    .await
                    .write_all(&resp.to_json_line().unwrap_or_default())
                    .await;
                let session_name = sess.name();
                tracing::info!(session_id = %session_id, session = %session_name, client_id, "client detached");
                if attach_mode != AttachMode::Bytes {
                    writer_task_handle.abort();
                    return;
                }
            }
            _ => break,
        }
    }

    sess.disconnect(client_id);
    writer_task_handle.abort();
    let session_name = sess.name();
    tracing::debug!(session_id = %session_id, session = %session_name, client_id, "client disconnected");
}

fn event_messages(session_id: &str, event: SessionEvent) -> Vec<Message> {
    match event {
        SessionEvent::Frame(frame) => vec![Message {
            kind: MessageKind::Event,
            event: Some(EventType::Frame),
            session_id: session_id.to_string(),
            frame: Some(frame),
            ..Default::default()
        }],
        SessionEvent::Output(output) => session::output_chunks(&output)
            .map(|chunk| Message {
                kind: MessageKind::Event,
                event: Some(EventType::Output),
                session_id: session_id.to_string(),
                payload: chunk.to_vec(),
                ..Default::default()
            })
            .collect(),
        SessionEvent::Exit(code) => vec![Message {
            kind: MessageKind::Event,
            event: Some(EventType::Exit),
            session_id: session_id.to_string(),
            exit_code: code as i64,
            ..Default::default()
        }],
    }
}

fn session_error(session_id: &str, e: anyhow::Error) -> anyhow::Error {
    let msg = e.to_string();
    if msg.contains("already attached") {
        anyhow::anyhow!("{}", attached_session_error(session_id))
    } else if msg.contains("exited") || msg.contains("closed") {
        anyhow::anyhow!("{}", exited_session_error(session_id))
    } else {
        e
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qscreen_protocol::AttachMode;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    fn test_state() -> Arc<State> {
        let (stop_tx, _stop_rx) = watch::channel(false);
        Arc::new(State::new(stop_tx))
    }

    async fn insert_session(state: &State, session_id: &str, name: &str) -> Arc<Session> {
        let session = Session::new(session_id.to_string(), name.to_string(), 80, 24, None).unwrap();
        state.insert_session(session_id.to_string(), session.clone());
        session
    }

    fn recv_exit(queue: &SessionEventQueue) -> i32 {
        for _ in 0..16 {
            match queue.try_recv().expect("expected session event") {
                SessionEvent::Frame(_) => {}
                SessionEvent::Output(_) => {}
                SessionEvent::Exit(code) => return code,
            }
        }
        panic!("expected exit");
    }

    async fn attach_with_script(
        state: Arc<State>,
        session_id: &str,
        attach_id: &str,
        attach_size: (u32, u32),
        commands: Vec<Message>,
    ) -> Vec<Message> {
        let (client, server) = tokio::io::duplex(4096);
        let handle = tokio::spawn(handle_connection(server, state));
        let mut reader = BufReader::new(client);

        let attach = Message {
            kind: MessageKind::Request,
            id: attach_id.to_string(),
            command: Some(Command::Attach),
            session_id: session_id.to_string(),
            width: attach_size.0,
            height: attach_size.1,
            ..Default::default()
        };
        reader
            .get_mut()
            .write_all(&attach.to_json_line().unwrap())
            .await
            .unwrap();

        let mut messages = vec![read_response_with_id(&mut reader, attach_id).await];

        for command in commands {
            let command_id = command.id.clone();
            reader
                .get_mut()
                .write_all(&command.to_json_line().unwrap())
                .await
                .unwrap();
            messages.push(read_response_with_id(&mut reader, &command_id).await);
        }

        drop(reader);
        handle.await.unwrap();
        messages
    }

    async fn read_response_with_id<S>(reader: &mut BufReader<S>, expected_id: &str) -> Message
    where
        S: tokio::io::AsyncRead + Unpin,
    {
        let mut line = String::new();
        loop {
            line.clear();
            reader.read_line(&mut line).await.unwrap();
            let msg = Message::from_json(&line).unwrap();
            if msg.kind == MessageKind::Response && msg.id == expected_id {
                return msg;
            }
        }
    }

    async fn read_message<S>(reader: &mut BufReader<S>) -> Message
    where
        S: tokio::io::AsyncRead + Unpin,
    {
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        Message::from_json(&line).unwrap()
    }

    #[tokio::test]
    async fn dispatch_new_assigns_incrementing_session_ids_and_default_name() {
        let state = test_state();

        let first = dispatch_command(
            &Message {
                kind: MessageKind::Request,
                id: "new-1".to_string(),
                command: Some(Command::New),
                width: 80,
                height: 24,
                ..Default::default()
            },
            &state,
        )
        .await;
        assert!(first.ok);
        assert_eq!(first.session_id, "1");
        assert_eq!(first.name, "1");

        let second = dispatch_command(
            &Message {
                kind: MessageKind::Request,
                id: "new-2".to_string(),
                command: Some(Command::New),
                name: "work".to_string(),
                width: 100,
                height: 30,
                ..Default::default()
            },
            &state,
        )
        .await;
        assert!(second.ok);
        assert_eq!(second.session_id, "2");
        assert_eq!(second.name, "work");

        let third = dispatch_command(
            &Message {
                kind: MessageKind::Request,
                id: "new-3".to_string(),
                command: Some(Command::New),
                name: "work".to_string(),
                width: 120,
                height: 40,
                ..Default::default()
            },
            &state,
        )
        .await;
        assert!(third.ok);
        assert_eq!(third.session_id, "3");
        assert_eq!(third.name, "work");

        let infos = state.list_sessions();
        assert_eq!(
            infos
                .iter()
                .map(|info| (info.session_id.as_str(), info.name.as_str()))
                .collect::<Vec<_>>(),
            vec![("1", "1"), ("2", "work"), ("3", "work")]
        );

        state.kill_all();
    }

    #[tokio::test]
    async fn dispatch_rename_changes_display_name_by_session_id() {
        let state = test_state();
        let session = insert_session(&state, "1", "old").await;

        let response = dispatch_command(
            &Message {
                kind: MessageKind::Request,
                id: "rename-1".to_string(),
                command: Some(Command::Rename),
                session_id: "1".to_string(),
                name: "new".to_string(),
                ..Default::default()
            },
            &state,
        )
        .await;

        assert!(response.ok);
        assert_eq!(response.session_id, "1");
        assert_eq!(response.name, "new");
        assert_eq!(session.name(), "new");
        assert_eq!(state.list_sessions()[0].name, "new");

        session.close();
    }

    #[tokio::test]
    async fn handle_attach_allows_second_client_and_reports_attached_in_list() {
        let state = test_state();
        let session = insert_session(&state, "1", "work").await;

        let (client, server) = tokio::io::duplex(4096);
        let handle = tokio::spawn(handle_connection(server, state.clone()));
        let mut reader = BufReader::new(client);

        let attach = Message {
            kind: MessageKind::Request,
            id: "attach-1".to_string(),
            command: Some(Command::Attach),
            session_id: "1".to_string(),
            width: 80,
            height: 24,
            ..Default::default()
        };
        reader
            .get_mut()
            .write_all(&attach.to_json_line().unwrap())
            .await
            .unwrap();
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let response = Message::from_json(&line).unwrap();
        assert!(response.ok);

        let messages = attach_with_script(state.clone(), "1", "attach-2", (100, 30), vec![]).await;

        assert!(messages[0].ok);
        assert_eq!(session.attached_clients.lock().unwrap().len(), 1);
        assert!(state.list_sessions()[0].attached);

        drop(reader);
        handle.await.unwrap();
        session.close();
    }

    #[tokio::test]
    async fn handle_attach_default_first_event_is_frame() {
        let state = test_state();
        let session = insert_session(&state, "1", "work").await;
        session.scrollback.lock().unwrap().append(b"history\r\n");

        let (client, server) = tokio::io::duplex(4096);
        let handle = tokio::spawn(handle_connection(server, state));
        let mut reader = BufReader::new(client);

        let attach = Message {
            kind: MessageKind::Request,
            id: "attach-1".to_string(),
            command: Some(Command::Attach),
            session_id: "1".to_string(),
            width: 80,
            height: 24,
            ..Default::default()
        };
        reader
            .get_mut()
            .write_all(&attach.to_json_line().unwrap())
            .await
            .unwrap();

        let response = read_message(&mut reader).await;
        assert!(response.ok);
        let replay_event = read_message(&mut reader).await;
        assert_eq!(replay_event.kind, MessageKind::Event);
        assert_eq!(replay_event.event, Some(EventType::Output));
        assert_eq!(replay_event.payload, b"history\r\n");
        let first_event = read_message(&mut reader).await;
        assert_eq!(first_event.kind, MessageKind::Event);
        assert_eq!(first_event.event, Some(EventType::Frame));
        assert!(first_event.frame.is_some());
        assert!(first_event.payload.is_empty());

        drop(reader);
        handle.await.unwrap();
        session.close();
    }

    #[tokio::test]
    async fn handle_attach_bytes_first_event_is_output_and_snapshot_chunks() {
        let state = test_state();
        let session = insert_session(&state, "1", "work").await;
        let snapshot = vec![b'x'; qscreen_protocol::MAX_PAYLOAD_SIZE + 3];
        session.scrollback.lock().unwrap().append(&snapshot);

        let (client, server) = tokio::io::duplex(512 * 1024);
        let handle = tokio::spawn(handle_connection(server, state));
        let mut reader = BufReader::new(client);

        let attach = Message {
            kind: MessageKind::Request,
            id: "attach-1".to_string(),
            command: Some(Command::Attach),
            session_id: "1".to_string(),
            width: 80,
            height: 24,
            attach_mode: AttachMode::Bytes,
            ..Default::default()
        };
        reader
            .get_mut()
            .write_all(&attach.to_json_line().unwrap())
            .await
            .unwrap();

        let response = read_message(&mut reader).await;
        assert!(response.ok);

        let first_event = read_message(&mut reader).await;
        assert_eq!(first_event.kind, MessageKind::Event);
        assert_eq!(first_event.event, Some(EventType::Output));
        assert_eq!(
            first_event.payload.len(),
            qscreen_protocol::MAX_PAYLOAD_SIZE
        );
        assert!(first_event.frame.is_none());

        let second_event = read_message(&mut reader).await;
        assert_eq!(second_event.kind, MessageKind::Event);
        assert_eq!(second_event.event, Some(EventType::Output));
        assert_eq!(second_event.payload.len(), 3);

        let mut replayed = first_event.payload;
        replayed.extend_from_slice(&second_event.payload);
        assert_eq!(replayed, snapshot);

        drop(reader);
        handle.await.unwrap();
        session.close();
    }

    #[tokio::test]
    async fn handle_attach_detach_removes_only_that_client() {
        let state = test_state();
        let session = insert_session(&state, "1", "work").await;
        let queue = SessionEventQueue::new();
        let (kept_client, _) = session.attach(queue, 80, 24, AttachMode::Frame).unwrap();

        let messages = attach_with_script(
            state.clone(),
            "1",
            "attach-1",
            (100, 30),
            vec![Message {
                kind: MessageKind::Request,
                id: "detach-1".to_string(),
                command: Some(Command::Detach),
                session_id: "1".to_string(),
                ..Default::default()
            }],
        )
        .await;

        assert!(messages[0].ok);
        assert!(messages[1].ok);
        let attached = session.attached_clients.lock().unwrap();
        assert_eq!(attached.len(), 1);
        assert!(attached.contains_key(&kept_client));
        drop(attached);

        session.close();
    }

    #[tokio::test]
    async fn handle_attach_bytes_detach_keeps_stream_and_resize_reattaches() {
        let state = test_state();
        let session = insert_session(&state, "1", "work").await;

        let (client, server) = tokio::io::duplex(4096);
        let handle = tokio::spawn(handle_connection(server, state.clone()));
        let mut reader = BufReader::new(client);

        let attach = Message {
            kind: MessageKind::Request,
            id: "attach-1".to_string(),
            command: Some(Command::Attach),
            session_id: "1".to_string(),
            width: 80,
            height: 24,
            attach_mode: AttachMode::Bytes,
            ..Default::default()
        };
        reader
            .get_mut()
            .write_all(&attach.to_json_line().unwrap())
            .await
            .unwrap();
        let attach_resp = read_response_with_id(&mut reader, "attach-1").await;
        assert!(attach_resp.ok);

        let detach = Message {
            kind: MessageKind::Request,
            id: "detach-1".to_string(),
            command: Some(Command::Detach),
            session_id: "1".to_string(),
            ..Default::default()
        };
        reader
            .get_mut()
            .write_all(&detach.to_json_line().unwrap())
            .await
            .unwrap();
        let detach_resp = read_response_with_id(&mut reader, "detach-1").await;
        assert!(detach_resp.ok);
        assert!(!state.list_sessions()[0].attached);
        assert_eq!(session.attached_clients.lock().unwrap().len(), 1);

        let resize = Message {
            kind: MessageKind::Request,
            id: "resize-1".to_string(),
            command: Some(Command::Resize),
            session_id: "1".to_string(),
            width: 100,
            height: 30,
            ..Default::default()
        };
        reader
            .get_mut()
            .write_all(&resize.to_json_line().unwrap())
            .await
            .unwrap();
        let resize_resp = read_response_with_id(&mut reader, "resize-1").await;
        assert!(resize_resp.ok);
        assert!(state.list_sessions()[0].attached);
        assert_eq!((session.width(), session.height()), (100, 30));

        drop(reader);
        handle.await.unwrap();
        session.close();
    }

    #[tokio::test]
    async fn handle_attach_rejects_invalid_resize_without_changing_session_size() {
        let state = test_state();
        let session = insert_session(&state, "1", "work").await;

        let (client, server) = tokio::io::duplex(4096);
        let handle = tokio::spawn(handle_connection(server, state));
        let mut reader = BufReader::new(client);

        let attach = Message {
            kind: MessageKind::Request,
            id: "attach-1".to_string(),
            command: Some(Command::Attach),
            session_id: "1".to_string(),
            width: 80,
            height: 24,
            attach_mode: AttachMode::Bytes,
            ..Default::default()
        };
        reader
            .get_mut()
            .write_all(&attach.to_json_line().unwrap())
            .await
            .unwrap();
        let attach_resp = read_response_with_id(&mut reader, "attach-1").await;
        assert!(attach_resp.ok);
        assert_eq!((session.width(), session.height()), (80, 24));

        for (id, width, height) in [("resize-width", 1001, 500), ("resize-height", 1000, 501)] {
            let resize = Message {
                kind: MessageKind::Request,
                id: id.to_string(),
                command: Some(Command::Resize),
                session_id: "1".to_string(),
                width,
                height,
                ..Default::default()
            };
            reader
                .get_mut()
                .write_all(&resize.to_json_line().unwrap())
                .await
                .unwrap();
            let resize_resp = read_response_with_id(&mut reader, id).await;
            assert!(!resize_resp.ok);
            assert!(!resize_resp.error.is_empty());
            assert_eq!((session.width(), session.height()), (80, 24));
        }

        drop(reader);
        handle.await.unwrap();
        session.close();
    }

    #[tokio::test]
    async fn dispatch_kill_notifies_all_attached_clients_and_removes_session() {
        let state = test_state();
        let session = insert_session(&state, "1", "work").await;
        let queue1 = SessionEventQueue::new();
        let queue2 = SessionEventQueue::new();

        let _ = session
            .attach(queue1.clone(), 80, 24, AttachMode::Frame)
            .unwrap();
        let _ = session
            .attach(queue2.clone(), 100, 30, AttachMode::Frame)
            .unwrap();

        let response = dispatch_command(
            &Message {
                kind: MessageKind::Request,
                id: "kill-1".to_string(),
                command: Some(Command::Kill),
                session_id: "1".to_string(),
                ..Default::default()
            },
            &state,
        )
        .await;

        assert!(response.ok);
        assert_eq!(response.id, "kill-1");
        assert!(state.get_session("1").is_none());
        assert_eq!(recv_exit(&queue1), -1);
        assert_eq!(recv_exit(&queue2), -1);
        assert!(!session.is_attached());
    }

    #[tokio::test]
    async fn exited_sessions_are_kept_as_list_tombstones() {
        let state = test_state();
        let session = insert_session(&state, "1", "work").await;

        session.close();

        for _ in 0..50 {
            if state.get_session("1").is_none() {
                let infos = state.list_sessions();
                assert_eq!(infos.len(), 1);
                assert_eq!(infos[0].session_id, "1");
                assert!(infos[0].exited);
                assert!(!infos[0].attached);
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("expected exited session tombstone");
    }

    #[tokio::test]
    async fn handle_attach_focus_applies_attached_client_size() {
        let state = test_state();
        let session = insert_session(&state, "1", "work").await;
        let queue = SessionEventQueue::new();
        let (first_client, _) = session.attach(queue, 80, 24, AttachMode::Frame).unwrap();

        let (client, server) = tokio::io::duplex(4096);
        let handle = tokio::spawn(handle_connection(server, state.clone()));
        let mut reader = BufReader::new(client);
        let mut line = String::new();

        let attach = Message {
            kind: MessageKind::Request,
            id: "attach-1".to_string(),
            command: Some(Command::Attach),
            session_id: "1".to_string(),
            width: 100,
            height: 30,
            ..Default::default()
        };
        reader
            .get_mut()
            .write_all(&attach.to_json_line().unwrap())
            .await
            .unwrap();
        reader.read_line(&mut line).await.unwrap();
        let attach_resp = Message::from_json(&line).unwrap();
        assert!(attach_resp.ok);
        line.clear();
        reader.read_line(&mut line).await.unwrap();
        let screen_event = Message::from_json(&line).unwrap();
        assert_eq!(screen_event.kind, MessageKind::Event);
        assert_eq!(screen_event.event, Some(EventType::Frame));
        assert!(screen_event.frame.is_some());
        assert!(screen_event.payload.is_empty());

        line.clear();
        let resize = Message {
            kind: MessageKind::Request,
            id: "resize-1".to_string(),
            command: Some(Command::Resize),
            session_id: "1".to_string(),
            width: 120,
            height: 40,
            ..Default::default()
        };
        reader
            .get_mut()
            .write_all(&resize.to_json_line().unwrap())
            .await
            .unwrap();
        reader.read_line(&mut line).await.unwrap();
        let resize_resp = Message::from_json(&line).unwrap();
        assert!(resize_resp.ok);

        session.focus_client(first_client).unwrap();
        assert_eq!((session.width(), session.height()), (80, 24));
        assert_eq!(
            *session.active_client_id.lock().unwrap(),
            Some(first_client)
        );

        line.clear();
        let focus = Message {
            kind: MessageKind::Request,
            id: "focus-1".to_string(),
            command: Some(Command::Focus),
            session_id: "1".to_string(),
            ..Default::default()
        };
        reader
            .get_mut()
            .write_all(&focus.to_json_line().unwrap())
            .await
            .unwrap();
        reader.read_line(&mut line).await.unwrap();
        let focus_resp = Message::from_json(&line).unwrap();
        assert!(focus_resp.ok);

        assert_eq!((session.width(), session.height()), (120, 40));
        assert_ne!(
            *session.active_client_id.lock().unwrap(),
            Some(first_client)
        );

        drop(reader);
        handle.await.unwrap();
        session.close();
    }
}
