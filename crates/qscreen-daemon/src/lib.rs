pub mod session;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::Context;
use qscreen_protocol::{
    Command, EventType, MAX_PAYLOAD_SIZE, Message, MessageKind, SessionInfo,
    attached_session_error, duplicate_session_error, exited_session_error, missing_session_error,
    validate_attach_size, validate_new_size, validate_resize, validate_session_name,
};
use qscreen_shared::pipe_name;
use session::{Session, SessionEvent};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot, watch};

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
        let mut infos: Vec<SessionInfo> = guard.values().map(|s| session_info(s)).collect();
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
                if let Some(existing) = guard.get(&msg.name)
                    && !existing.exited.load(std::sync::atomic::Ordering::SeqCst)
                {
                    anyhow::bail!("{}", duplicate_session_error(&msg.name));
                }
            }
            let sess = Session::new(
                msg.name.clone(),
                msg.width,
                msg.height,
                Some(msg.shell.as_str()),
            )?;
            tracing::info!(session = %msg.name, "session created");
            state
                .sessions
                .lock()
                .unwrap()
                .insert(msg.name.clone(), sess);
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
async fn handle_attach<R, W>(
    msg: Message,
    mut reader: BufReader<R>,
    writer: SharedWriter<W>,
    state: Arc<State>,
) where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
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
    let (client_id, scrollback) = match sess.attach(tx, msg.width, msg.height) {
        Ok(result) => result,
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

    tracing::info!(session = %session_name, client_id, "client attached");

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
        sess.detach(client_id);
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
            sess.detach(client_id);
            return;
        }
    }

    // 启动 writer 任务：PTY 输出 → client
    let (writer_done_tx, mut writer_done_rx) = oneshot::channel::<()>();
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
                tracing::debug!(session = %session_name, "parse error: {}", e);
                break;
            }
        };

        match &cmd.command {
            Some(Command::Focus) => {
                if let Err(e) = sess.focus_client(client_id) {
                    tracing::debug!(session = %session_name, client_id, "focus error: {}", e);
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
            Some(Command::Input) => {
                if let Err(e) = sess.input_client(client_id, &cmd.payload) {
                    tracing::debug!(session = %session_name, "write input error: {}", e);
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
                    let _ = sess.resize_client(client_id, cmd.width, cmd.height);
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
                sess.detach(client_id);
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
                tracing::info!(session = %session_name, client_id, "client detached");
                writer_task_handle.abort();
                return;
            }
            _ => break,
        }
    }

    sess.detach(client_id);
    writer_task_handle.abort();
    tracing::debug!(session = %session_name, client_id, "client disconnected");
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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    fn test_state() -> Arc<State> {
        let (stop_tx, _stop_rx) = watch::channel(false);
        Arc::new(State::new(stop_tx))
    }

    async fn insert_session(state: &State, name: &str) -> Arc<Session> {
        let session = Session::new(name.to_string(), 80, 24, None).unwrap();
        state
            .sessions
            .lock()
            .unwrap()
            .insert(name.to_string(), session.clone());
        session
    }

    fn recv_exit(rx: &mut mpsc::UnboundedReceiver<SessionEvent>) -> i32 {
        for _ in 0..16 {
            match rx.try_recv().expect("expected session event") {
                SessionEvent::Output(_) => {}
                SessionEvent::Exit(code) => return code,
            }
        }
        panic!("expected exit");
    }

    async fn attach_with_script(
        state: Arc<State>,
        name: &str,
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
            name: name.to_string(),
            width: attach_size.0,
            height: attach_size.1,
            ..Default::default()
        };
        reader
            .get_mut()
            .write_all(&attach.to_json_line().unwrap())
            .await
            .unwrap();

        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let mut messages = vec![Message::from_json(&line).unwrap()];

        for command in commands {
            reader
                .get_mut()
                .write_all(&command.to_json_line().unwrap())
                .await
                .unwrap();
            line.clear();
            reader.read_line(&mut line).await.unwrap();
            messages.push(Message::from_json(&line).unwrap());
        }

        drop(reader);
        handle.await.unwrap();
        messages
    }

    #[tokio::test]
    async fn handle_attach_allows_second_client_and_reports_attached_in_list() {
        let state = test_state();
        let session = insert_session(&state, "work").await;

        let (client, server) = tokio::io::duplex(4096);
        let handle = tokio::spawn(handle_connection(server, state.clone()));
        let mut reader = BufReader::new(client);

        let attach = Message {
            kind: MessageKind::Request,
            id: "attach-1".to_string(),
            command: Some(Command::Attach),
            name: "work".to_string(),
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

        let messages =
            attach_with_script(state.clone(), "work", "attach-2", (100, 30), vec![]).await;

        assert!(messages[0].ok);
        assert_eq!(session.attached_clients.lock().unwrap().len(), 1);
        assert!(state.list_sessions()[0].attached);

        drop(reader);
        handle.await.unwrap();
        session.close();
    }

    #[tokio::test]
    async fn handle_attach_detach_removes_only_that_client() {
        let state = test_state();
        let session = insert_session(&state, "work").await;
        let (tx, _rx) = mpsc::unbounded_channel();
        let (kept_client, _) = session.attach(tx, 80, 24).unwrap();

        let messages = attach_with_script(
            state.clone(),
            "work",
            "attach-1",
            (100, 30),
            vec![Message {
                kind: MessageKind::Request,
                id: "detach-1".to_string(),
                command: Some(Command::Detach),
                name: "work".to_string(),
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
    async fn dispatch_kill_notifies_all_attached_clients_and_removes_session() {
        let state = test_state();
        let session = insert_session(&state, "work").await;
        let (tx1, mut rx1) = mpsc::unbounded_channel();
        let (tx2, mut rx2) = mpsc::unbounded_channel();

        let _ = session.attach(tx1, 80, 24).unwrap();
        let _ = session.attach(tx2, 100, 30).unwrap();

        let response = dispatch_command(
            &Message {
                kind: MessageKind::Request,
                id: "kill-1".to_string(),
                command: Some(Command::Kill),
                name: "work".to_string(),
                ..Default::default()
            },
            &state,
        )
        .await;

        assert!(response.ok);
        assert_eq!(response.id, "kill-1");
        assert!(state.get_session("work").is_none());
        assert_eq!(recv_exit(&mut rx1), -1);
        assert_eq!(recv_exit(&mut rx2), -1);
        assert!(!session.is_attached());
    }

    #[tokio::test]
    async fn handle_attach_focus_applies_attached_client_size() {
        let state = test_state();
        let session = insert_session(&state, "work").await;
        let (tx, _rx) = mpsc::unbounded_channel();
        let (first_client, _) = session.attach(tx, 80, 24).unwrap();

        let (client, server) = tokio::io::duplex(4096);
        let handle = tokio::spawn(handle_connection(server, state.clone()));
        let mut reader = BufReader::new(client);
        let mut line = String::new();

        let attach = Message {
            kind: MessageKind::Request,
            id: "attach-1".to_string(),
            command: Some(Command::Attach),
            name: "work".to_string(),
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
        let resize = Message {
            kind: MessageKind::Request,
            id: "resize-1".to_string(),
            command: Some(Command::Resize),
            name: "work".to_string(),
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
            name: "work".to_string(),
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
