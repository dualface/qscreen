use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Context;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, NativePtySystem, PtySize, PtySystem};
use qscreen_protocol::{
    FRAME_FLAG_BOLD, FRAME_FLAG_INVERSE, FRAME_FLAG_ITALIC, FRAME_FLAG_UNDERLINE, FrameColor,
    ScreenFrame, ScreenRun,
};
use tokio::sync::mpsc;

pub const SCROLLBACK_LIMIT: usize = 256 * 1024;
const DEFAULT_WIDTH: u16 = 80;
const DEFAULT_HEIGHT: u16 = 24;
#[cfg(any(windows, test))]
const DEFAULT_WINDOWS_SHELL: &str = r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe";
#[cfg(any(windows, test))]
const CMD_WINDOWS_SHELL: &str = r"C:\Windows\System32\cmd.exe";
const TERM_XTERM_256COLOR: &str = "xterm-256color";
const COLOR_TERM_TRUECOLOR: &str = "truecolor";

pub type ClientId = u64;

/// PTY 输出事件，通过 attached_clients 发给当前 attach 的客户端
#[derive(Debug)]
pub enum SessionEvent {
    Output(Bytes),
    Exit(i32),
}

pub struct AttachedClient {
    pub tx: mpsc::UnboundedSender<SessionEvent>,
    pub width: u16,
    pub height: u16,
}

/// 256KB 环形 scrollback buffer（字节级）
pub struct ScrollbackBuf {
    data: Vec<u8>,
}

impl ScrollbackBuf {
    fn new() -> Self {
        ScrollbackBuf { data: Vec::new() }
    }

    pub fn append(&mut self, p: &[u8]) {
        if p.len() >= SCROLLBACK_LIMIT {
            self.data.clear();
            self.data
                .extend_from_slice(&p[p.len() - SCROLLBACK_LIMIT..]);
            return;
        }
        self.data.extend_from_slice(p);
        if self.data.len() > SCROLLBACK_LIMIT {
            let over = self.data.len() - SCROLLBACK_LIMIT;
            self.data.drain(..over);
        }
    }

    pub fn snapshot(&self) -> Vec<u8> {
        self.data.clone()
    }
}

pub struct Session {
    pub session_id: String,
    name: Arc<Mutex<String>>,
    pub created_at: DateTime<Utc>,
    width: Arc<Mutex<u16>>,
    height: Arc<Mutex<u16>>,
    pub exited: Arc<AtomicBool>,
    pub exit_code: Arc<Mutex<Option<i32>>>,
    pub closed: Arc<AtomicBool>,
    /// PTY master：仅用于 resize（take_writer 后 write 走 pty_writer）
    pty_master: Arc<Mutex<Option<Box<dyn MasterPty + Send>>>>,
    /// PTY writer：写 input
    pty_writer: Arc<Mutex<Option<Box<dyn Write + Send>>>>,
    child_killer: Arc<Mutex<Option<Box<dyn ChildKiller + Send + Sync>>>>,
    pub scrollback: Arc<Mutex<ScrollbackBuf>>,
    screen: Arc<Mutex<vt100::Parser>>,
    /// 已 attach 客户端；空 map = detached
    pub attached_clients: Arc<Mutex<HashMap<ClientId, AttachedClient>>>,
    pub active_client_id: Arc<Mutex<Option<ClientId>>>,
    next_client_id: Arc<Mutex<ClientId>>,
}

impl Session {
    pub fn new(
        session_id: String,
        name: String,
        width: u32,
        height: u32,
        shell: Option<&str>,
    ) -> anyhow::Result<Arc<Self>> {
        let w = if width > 0 {
            width as u16
        } else {
            DEFAULT_WIDTH
        };
        let h = if height > 0 {
            height as u16
        } else {
            DEFAULT_HEIGHT
        };

        let pty_system = NativePtySystem::default();
        let pair = pty_system
            .openpty(PtySize {
                rows: h,
                cols: w,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("openpty failed")?;

        // 先拿 reader 和 writer，再把 master 包进 Arc<Mutex>
        let pty_reader = pair.master.try_clone_reader().context("try_clone_reader")?;
        let pty_writer = pair.master.take_writer().context("take_writer")?;

        let cmd = default_shell_command(shell).context("resolve shell command")?;
        let child = pair.slave.spawn_command(cmd).context("spawn shell")?;
        let child_killer = Arc::new(Mutex::new(Some(child.clone_killer())));
        drop(pair.slave);

        let scrollback = Arc::new(Mutex::new(ScrollbackBuf::new()));
        let attached_clients: Arc<Mutex<HashMap<ClientId, AttachedClient>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let active_client_id: Arc<Mutex<Option<ClientId>>> = Arc::new(Mutex::new(None));
        let next_client_id = Arc::new(Mutex::new(1));
        let exited = Arc::new(AtomicBool::new(false));
        let exit_code: Arc<Mutex<Option<i32>>> = Arc::new(Mutex::new(None));
        let closed = Arc::new(AtomicBool::new(false));
        let pty_master = Arc::new(Mutex::new(Some(pair.master)));
        let pty_writer_arc = Arc::new(Mutex::new(Some(pty_writer)));
        let screen = Arc::new(Mutex::new(vt100::Parser::new(h, w, 0)));

        let sess = Arc::new(Session {
            session_id,
            name: Arc::new(Mutex::new(name.clone())),
            created_at: Utc::now(),
            width: Arc::new(Mutex::new(w)),
            height: Arc::new(Mutex::new(h)),
            exited: exited.clone(),
            exit_code: exit_code.clone(),
            closed: closed.clone(),
            pty_master: pty_master.clone(),
            pty_writer: pty_writer_arc.clone(),
            child_killer: child_killer.clone(),
            scrollback: scrollback.clone(),
            screen: screen.clone(),
            attached_clients: attached_clients.clone(),
            active_client_id: active_client_id.clone(),
            next_client_id,
        });

        // PTY output 读取任务（阻塞 IO，放在 spawn_blocking）
        {
            let scrollback_r = scrollback.clone();
            let screen_r = screen.clone();
            let attached_r = attached_clients.clone();
            let active_client_r = active_client_id.clone();
            let exited_r = exited.clone();
            let name_r = name.clone();
            tokio::task::spawn_blocking(move || {
                let mut reader = pty_reader;
                let mut buf = vec![0u8; 32 * 1024];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let data = Bytes::copy_from_slice(&buf[..n]);
                            scrollback_r.lock().unwrap().append(&data);
                            screen_r.lock().unwrap().process(&data);
                            broadcast_output(&attached_r, &active_client_r, data);
                        }
                    }
                }
                tracing::debug!(session = %name_r, "pty reader ended");
                exited_r.store(true, Ordering::SeqCst);
            });
        }

        // 子进程退出等待任务
        {
            let attached_e = attached_clients.clone();
            let active_client_e = active_client_id.clone();
            let exited_e = exited.clone();
            let exit_code_e = exit_code.clone();
            let name_e = name.clone();
            tokio::task::spawn_blocking(move || {
                let mut child = child;
                let code = match child.wait() {
                    Ok(status) => status.exit_code() as i32,
                    Err(_) => -1,
                };
                tracing::info!(session = %name_e, exit_code = code, "session exited");
                *exit_code_e.lock().unwrap() = Some(code);
                exited_e.store(true, Ordering::SeqCst);
                let mut attached = attached_e.lock().unwrap();
                for (_, client) in attached.drain() {
                    let _ = client.tx.send(SessionEvent::Exit(code));
                }
                *active_client_e.lock().unwrap() = None;
            });
        }

        Ok(sess)
    }

    pub fn name(&self) -> String {
        self.name.lock().unwrap().clone()
    }

    pub fn rename(&self, name: String) {
        *self.name.lock().unwrap() = name;
    }

    pub fn width(&self) -> u16 {
        *self.width.lock().unwrap()
    }

    pub fn height(&self) -> u16 {
        *self.height.lock().unwrap()
    }

    fn ensure_open(&self) -> anyhow::Result<()> {
        if self.closed.load(Ordering::SeqCst) {
            anyhow::bail!("session is closed");
        }
        Ok(())
    }

    fn resize_pty(&self, width: u16, height: u16) -> anyhow::Result<()> {
        self.ensure_open()?;
        let guard = self.pty_master.lock().unwrap();
        match guard.as_ref() {
            None => anyhow::bail!("session is closed"),
            Some(m) => {
                m.resize(PtySize {
                    rows: height,
                    cols: width,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .context("resize pty")?;
            }
        }
        drop(guard);
        *self.width.lock().unwrap() = width;
        *self.height.lock().unwrap() = height;
        self.screen.lock().unwrap().set_size(height, width);
        Ok(())
    }

    fn client_size(&self, client_id: ClientId) -> anyhow::Result<(u16, u16)> {
        let attached = self.attached_clients.lock().unwrap();
        let client = attached
            .get(&client_id)
            .ok_or_else(|| anyhow::anyhow!("client {client_id} is not attached"))?;
        Ok((client.width, client.height))
    }

    /// 写输入到 PTY（forwarded from client Input 命令）
    pub fn write_input(&self, data: &[u8]) -> anyhow::Result<()> {
        self.ensure_open()?;
        let mut guard = self.pty_writer.lock().unwrap();
        match guard.as_mut() {
            None => anyhow::bail!("session is closed"),
            Some(w) => {
                w.write_all(data).context("write to pty")?;
                Ok(())
            }
        }
    }

    /// resize PTY
    pub fn resize(&self, width: u32, height: u32) -> anyhow::Result<()> {
        self.resize_pty(width as u16, height as u16)
    }

    /// Attach 一个客户端：返回当前 screen frame + 注册事件发送端
    /// 返回 Err 如果 session 已退出
    pub fn attach(
        &self,
        tx: mpsc::UnboundedSender<SessionEvent>,
        width: u32,
        height: u32,
    ) -> anyhow::Result<(ClientId, ScreenFrame)> {
        if self.exited.load(Ordering::SeqCst) {
            anyhow::bail!("session has exited");
        }
        if self.closed.load(Ordering::SeqCst) {
            anyhow::bail!("session is closed");
        }
        self.resize(width, height)?;

        let screen_frame = self.screen_frame();
        let mut next_id = self.next_client_id.lock().unwrap();
        let client_id = *next_id;
        *next_id += 1;
        drop(next_id);

        let w = width as u16;
        let h = height as u16;
        let mut guard = self.attached_clients.lock().unwrap();
        guard.insert(
            client_id,
            AttachedClient {
                tx,
                width: w,
                height: h,
            },
        );
        *self.active_client_id.lock().unwrap() = Some(client_id);
        Ok((client_id, screen_frame))
    }

    fn screen_frame(&self) -> ScreenFrame {
        let parser = self.screen.lock().unwrap();
        let screen = parser.screen();
        let (rows, cols) = screen.size();
        let (cursor_row, cursor_col) = screen.cursor_position();
        let mut rows_v2 = Vec::with_capacity(rows as usize);

        for row in 0..rows {
            let mut runs = Vec::new();
            let mut current: Option<ScreenRun> = None;

            for col in 0..cols {
                let Some(cell) = screen.cell(row, col) else {
                    push_cell_run(
                        &mut runs,
                        &mut current,
                        " ".to_string(),
                        1,
                        default_run_attrs(),
                    );
                    continue;
                };
                if cell.is_wide_continuation() {
                    continue;
                }

                let text = if cell.has_contents() {
                    cell.contents()
                } else {
                    " ".to_string()
                };
                let width = if cell.is_wide() { 2 } else { 1 };
                let attrs = cell_run_attrs(cell);
                push_cell_run(&mut runs, &mut current, text, width, attrs);
            }

            if let Some(run) = current.take() {
                runs.push(run);
            }
            rows_v2.push(runs);
        }

        ScreenFrame {
            rows,
            cols,
            cursor_row,
            cursor_col,
            hide_cursor: screen.hide_cursor(),
            alternate_screen: screen.alternate_screen(),
            rows_v2,
        }
    }

    pub fn focus_client(&self, client_id: ClientId) -> anyhow::Result<()> {
        let (width, height) = self.client_size(client_id)?;
        *self.active_client_id.lock().unwrap() = Some(client_id);
        self.resize_pty(width, height)
    }

    pub fn input_client(&self, client_id: ClientId, data: &[u8]) -> anyhow::Result<()> {
        self.focus_client(client_id)?;
        self.write_input(data)
    }

    pub fn resize_client(
        &self,
        client_id: ClientId,
        width: u32,
        height: u32,
    ) -> anyhow::Result<()> {
        self.ensure_open()?;
        {
            let mut attached = self.attached_clients.lock().unwrap();
            let client = attached
                .get_mut(&client_id)
                .ok_or_else(|| anyhow::anyhow!("client {client_id} is not attached"))?;
            client.width = width as u16;
            client.height = height as u16;
        }
        if *self.active_client_id.lock().unwrap() == Some(client_id) {
            self.resize_pty(width as u16, height as u16)?;
        }
        Ok(())
    }

    /// Detach 指定客户端（幂等）
    pub fn detach(&self, client_id: ClientId) {
        self.attached_clients.lock().unwrap().remove(&client_id);
        if *self.active_client_id.lock().unwrap() == Some(client_id) {
            *self.active_client_id.lock().unwrap() = None;
        }
    }

    /// 关闭 session（kill PTY）
    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        if let Some(mut killer) = self.child_killer.lock().unwrap().take() {
            let _ = killer.kill();
        }
        // 丢弃 writer 和 master → PTY 管道关闭 → reader task 结束
        self.pty_writer.lock().unwrap().take();
        self.pty_master.lock().unwrap().take();
        // 通知已 attach 的客户端
        let mut attached = self.attached_clients.lock().unwrap();
        for (_, client) in attached.drain() {
            let _ = client.tx.send(SessionEvent::Exit(-1));
        }
        *self.active_client_id.lock().unwrap() = None;
    }

    pub fn is_attached(&self) -> bool {
        !self.attached_clients.lock().unwrap().is_empty()
    }
}

fn push_cell_run(
    runs: &mut Vec<ScreenRun>,
    current: &mut Option<ScreenRun>,
    text: String,
    width: u16,
    attrs: (FrameColor, FrameColor, u8),
) {
    let (fg, bg, flags) = attrs;
    match current.as_mut() {
        Some(run) if run.fg == fg && run.bg == bg && run.flags == flags => {
            run.text.push_str(&text);
            run.width = run.width.saturating_add(width);
        }
        _ => {
            if let Some(run) = current.take() {
                runs.push(run);
            }
            *current = Some(ScreenRun {
                text,
                fg,
                bg,
                flags,
                width,
            });
        }
    }
}

fn default_run_attrs() -> (FrameColor, FrameColor, u8) {
    (FrameColor::Default, FrameColor::Default, 0)
}

fn cell_run_attrs(cell: &vt100::Cell) -> (FrameColor, FrameColor, u8) {
    let mut flags = 0;
    if cell.bold() {
        flags |= FRAME_FLAG_BOLD;
    }
    if cell.italic() {
        flags |= FRAME_FLAG_ITALIC;
    }
    if cell.underline() {
        flags |= FRAME_FLAG_UNDERLINE;
    }
    if cell.inverse() {
        flags |= FRAME_FLAG_INVERSE;
    }
    (
        frame_color(cell.fgcolor()),
        frame_color(cell.bgcolor()),
        flags,
    )
}

fn frame_color(color: vt100::Color) -> FrameColor {
    match color {
        vt100::Color::Default => FrameColor::Default,
        vt100::Color::Idx(value) => FrameColor::Idx(value),
        vt100::Color::Rgb(r, g, b) => FrameColor::Rgb(r, g, b),
    }
}

fn broadcast_output(
    attached_clients: &Arc<Mutex<HashMap<ClientId, AttachedClient>>>,
    active_client_id: &Arc<Mutex<Option<ClientId>>>,
    data: Bytes,
) {
    let failed_clients = {
        let attached = attached_clients.lock().unwrap();
        attached
            .iter()
            .filter_map(|(client_id, client)| {
                client
                    .tx
                    .send(SessionEvent::Output(data.clone()))
                    .err()
                    .map(|_| *client_id)
            })
            .collect::<Vec<_>>()
    };
    if failed_clients.is_empty() {
        return;
    }

    let mut attached = attached_clients.lock().unwrap();
    let mut active = active_client_id.lock().unwrap();
    for client_id in failed_clients {
        attached.remove(&client_id);
        if *active == Some(client_id) {
            *active = None;
        }
    }
}

#[cfg(any(windows, test))]
fn resolve_windows_shell_preference(preference: Option<&str>) -> anyhow::Result<&'static str> {
    match preference.map(str::trim).filter(|value| !value.is_empty()) {
        None => Ok(DEFAULT_WINDOWS_SHELL),
        Some("cmd" | "cmd.exe") => Ok(CMD_WINDOWS_SHELL),
        Some("powershell" | "powershell.exe") => Ok(DEFAULT_WINDOWS_SHELL),
        Some(value) => anyhow::bail!("unsupported Windows shell value: {value}"),
    }
}

fn default_shell_command(shell: Option<&str>) -> anyhow::Result<CommandBuilder> {
    #[cfg(windows)]
    {
        let env_preference = std::env::var("QSCREEN_WINDOWS_SHELL").ok();
        let preference = shell
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .or(env_preference.as_deref());
        let shell_path = resolve_windows_shell_preference(preference)?;
        let mut cmd = CommandBuilder::new(shell_path);
        cmd.env("TERM", TERM_XTERM_256COLOR);
        cmd.env("COLORTERM", COLOR_TERM_TRUECOLOR);
        Ok(cmd)
    }
    #[cfg(unix)]
    {
        let shell_path = shell
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .or_else(|| std::env::var("SHELL").ok())
            .unwrap_or_else(|| "/bin/sh".to_string());
        let mut cmd = CommandBuilder::new(shell_path);
        cmd.env("TERM", TERM_XTERM_256COLOR);
        cmd.env("COLORTERM", COLOR_TERM_TRUECOLOR);
        cmd.arg("-l");
        Ok(cmd)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn windows_shell_preference_defaults_to_powershell_for_unset_or_empty() {
        assert_eq!(
            resolve_windows_shell_preference(None).unwrap(),
            DEFAULT_WINDOWS_SHELL
        );
        assert_eq!(
            resolve_windows_shell_preference(Some("")).unwrap(),
            DEFAULT_WINDOWS_SHELL
        );
        assert_eq!(
            resolve_windows_shell_preference(Some("  ")).unwrap(),
            DEFAULT_WINDOWS_SHELL
        );
    }

    #[test]
    fn windows_shell_preference_resolves_cmd_aliases() {
        assert_eq!(
            resolve_windows_shell_preference(Some("cmd")).unwrap(),
            CMD_WINDOWS_SHELL
        );
        assert_eq!(
            resolve_windows_shell_preference(Some("cmd.exe")).unwrap(),
            CMD_WINDOWS_SHELL
        );
    }

    #[test]
    fn windows_shell_preference_resolves_powershell_aliases() {
        assert_eq!(
            resolve_windows_shell_preference(Some("powershell")).unwrap(),
            DEFAULT_WINDOWS_SHELL
        );
        assert_eq!(
            resolve_windows_shell_preference(Some("powershell.exe")).unwrap(),
            DEFAULT_WINDOWS_SHELL
        );
    }

    #[test]
    fn windows_shell_preference_rejects_unsupported_values() {
        let err = resolve_windows_shell_preference(Some("pwsh"))
            .expect_err("unsupported shell should fail")
            .to_string();

        assert!(err.contains("unsupported Windows shell value: pwsh"));
    }

    #[test]
    fn default_shell_command_sets_xterm_256color_env() {
        let cmd = default_shell_command(None).expect("default shell command should build");
        assert_eq!(cmd.get_env("TERM"), Some(OsStr::new(TERM_XTERM_256COLOR)));
        assert_eq!(
            cmd.get_env("COLORTERM"),
            Some(OsStr::new(COLOR_TERM_TRUECOLOR))
        );
    }

    fn recv_output_matching(rx: &mut mpsc::UnboundedReceiver<SessionEvent>, expected: &[u8]) {
        for _ in 0..16 {
            match rx.try_recv().expect("expected session event") {
                SessionEvent::Output(data) if data == expected => return,
                SessionEvent::Output(_) => {}
                SessionEvent::Exit(code) => panic!("expected output, got exit {code}"),
            }
        }
        panic!("expected matching output");
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

    #[tokio::test]
    async fn attach_multiple_clients_and_detach_independently() {
        let session =
            Session::new("1".to_string(), "multi-detach".to_string(), 80, 24, None).unwrap();
        let (tx1, _rx1) = mpsc::unbounded_channel();
        let (tx2, _rx2) = mpsc::unbounded_channel();

        let (client1, frame1) = session.attach(tx1, 80, 24).unwrap();
        let (client2, frame2) = session.attach(tx2, 100, 30).unwrap();

        assert_ne!(client1, client2);
        assert_eq!((frame1.cols, frame1.rows), (80, 24));
        assert_eq!((frame2.cols, frame2.rows), (100, 30));
        assert!(session.is_attached());
        assert_eq!(session.attached_clients.lock().unwrap().len(), 2);
        assert_eq!(*session.active_client_id.lock().unwrap(), Some(client2));

        session.detach(client1);
        assert!(session.is_attached());
        assert_eq!(session.attached_clients.lock().unwrap().len(), 1);
        assert!(
            session
                .attached_clients
                .lock()
                .unwrap()
                .contains_key(&client2)
        );

        session.close();
    }

    #[tokio::test]
    async fn broadcast_output_reaches_all_clients_and_removes_only_failed_senders() {
        let session =
            Session::new("1".to_string(), "multi-broadcast".to_string(), 80, 24, None).unwrap();
        let (tx1, mut rx1) = mpsc::unbounded_channel();
        let (tx2, rx2) = mpsc::unbounded_channel();
        let (tx3, mut rx3) = mpsc::unbounded_channel();

        let (client1, _) = session.attach(tx1, 80, 24).unwrap();
        let (failed_client, _) = session.attach(tx2, 80, 24).unwrap();
        let (client3, _) = session.attach(tx3, 80, 24).unwrap();
        drop(rx2);

        broadcast_output(
            &session.attached_clients,
            &session.active_client_id,
            Bytes::from_static(b"chunk"),
        );

        recv_output_matching(&mut rx1, b"chunk");
        recv_output_matching(&mut rx3, b"chunk");
        let attached = session.attached_clients.lock().unwrap();
        assert!(attached.contains_key(&client1));
        assert!(!attached.contains_key(&failed_client));
        assert!(attached.contains_key(&client3));
        drop(attached);

        session.close();
    }

    #[tokio::test]
    async fn close_notifies_all_attached_clients() {
        let session =
            Session::new("1".to_string(), "multi-close".to_string(), 80, 24, None).unwrap();
        let (tx1, mut rx1) = mpsc::unbounded_channel();
        let (tx2, mut rx2) = mpsc::unbounded_channel();

        let _ = session.attach(tx1, 80, 24).unwrap();
        let _ = session.attach(tx2, 80, 24).unwrap();

        session.close();

        assert_eq!(recv_exit(&mut rx1), -1);
        assert_eq!(recv_exit(&mut rx2), -1);
        assert!(!session.is_attached());
        assert_eq!(*session.active_client_id.lock().unwrap(), None);
    }

    #[tokio::test]
    async fn inactive_resize_stores_size_without_pty_resize_until_focus_or_input() {
        let session =
            Session::new("1".to_string(), "multi-size".to_string(), 80, 24, None).unwrap();
        let (tx1, _rx1) = mpsc::unbounded_channel();
        let (tx2, _rx2) = mpsc::unbounded_channel();

        let (client1, _) = session.attach(tx1, 80, 24).unwrap();
        let (client2, _) = session.attach(tx2, 100, 30).unwrap();

        assert_eq!(*session.active_client_id.lock().unwrap(), Some(client2));
        assert_eq!((session.width(), session.height()), (100, 30));

        session.resize_client(client1, 120, 40).unwrap();

        assert_eq!(*session.active_client_id.lock().unwrap(), Some(client2));
        assert_eq!((session.width(), session.height()), (100, 30));
        {
            let attached = session.attached_clients.lock().unwrap();
            let resized = attached.get(&client1).unwrap();
            assert_eq!((resized.width, resized.height), (120, 40));
        }

        session.focus_client(client1).unwrap();

        assert_eq!(*session.active_client_id.lock().unwrap(), Some(client1));
        assert_eq!((session.width(), session.height()), (120, 40));

        session.close();
    }

    #[tokio::test]
    async fn input_client_marks_active_and_applies_client_size() {
        let session = Session::new(
            "1".to_string(),
            "multi-input-size".to_string(),
            80,
            24,
            None,
        )
        .unwrap();
        let (tx1, _rx1) = mpsc::unbounded_channel();
        let (tx2, _rx2) = mpsc::unbounded_channel();

        let (client1, _) = session.attach(tx1, 80, 24).unwrap();
        let (client2, _) = session.attach(tx2, 100, 30).unwrap();
        session.resize_client(client1, 90, 20).unwrap();

        assert_eq!(*session.active_client_id.lock().unwrap(), Some(client2));
        assert_eq!((session.width(), session.height()), (100, 30));

        session.input_client(client1, b"").unwrap();

        assert_eq!(*session.active_client_id.lock().unwrap(), Some(client1));
        assert_eq!((session.width(), session.height()), (90, 20));

        session.close();
    }

    #[tokio::test]
    async fn attach_returns_current_screen_frame() {
        let session = Session::new("1".to_string(), "frame".to_string(), 80, 24, None).unwrap();
        session
            .scrollback
            .lock()
            .unwrap()
            .append(b"old history\r\n");
        {
            let mut screen = session.screen.lock().unwrap();
            screen.process(b"\x1b[2J\x1b[Hcurrent");
        }
        let (tx, _rx) = mpsc::unbounded_channel();

        let (_, frame) = session.attach(tx, 80, 24).unwrap();
        let frame_text = frame
            .rows_v2
            .iter()
            .flat_map(|row| row.iter())
            .map(|run| run.text.as_str())
            .collect::<String>();

        assert_eq!((frame.cols, frame.rows), (80, 24));
        assert!(frame_text.contains("current"));
        assert!(!frame_text.contains("old history"));

        session.close();
    }
}
