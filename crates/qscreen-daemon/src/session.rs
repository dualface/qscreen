use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Context;
use chrono::{DateTime, Utc};
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, NativePtySystem, PtySize, PtySystem};
use qscreen_protocol::{
    AttachMode, FRAME_FLAG_BLINK, FRAME_FLAG_BOLD, FRAME_FLAG_DIM, FRAME_FLAG_HIDDEN,
    FRAME_FLAG_INVERSE, FRAME_FLAG_ITALIC, FRAME_FLAG_STRIKETHROUGH, FRAME_FLAG_UNDERLINE,
    FrameColor, FrameMouseEncoding, FrameMouseMode, MAX_PAYLOAD_SIZE, ScreenFrame, ScreenRun,
};
use tokio::sync::{Notify, watch};

pub const SCROLLBACK_LIMIT: usize = 256 * 1024;
const FRAME_CHANNEL_CAPACITY: usize = 16;
const CURSOR_SHAPE_DEFAULT: u8 = 0;
const DEFAULT_WIDTH: u16 = 80;
const DEFAULT_HEIGHT: u16 = 24;
#[cfg(any(windows, test))]
const DEFAULT_WINDOWS_SHELL: &str = r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe";
#[cfg(any(windows, test))]
const CMD_WINDOWS_SHELL: &str = r"C:\Windows\System32\cmd.exe";
const TERM_XTERM_256COLOR: &str = "xterm-256color";
const COLOR_TERM_TRUECOLOR: &str = "truecolor";

pub type ClientId = u64;

#[derive(Debug)]
pub enum SessionEvent {
    Frame(ScreenFrame),
    Output(Vec<u8>),
    Exit(i32),
}

pub struct AttachedClient {
    pub queue: Arc<SessionEventQueue>,
    pub width: u16,
    pub height: u16,
    pub attach_mode: AttachMode,
    pub attached: bool,
}

pub struct SessionEventQueue {
    inner: Mutex<SessionEventQueueInner>,
    notify: Notify,
}

struct SessionEventQueueInner {
    events: VecDeque<SessionEvent>,
    pending_exit: Option<i32>,
    closed: bool,
}

impl SessionEventQueue {
    pub fn new() -> Arc<Self> {
        Arc::new(SessionEventQueue {
            inner: Mutex::new(SessionEventQueueInner {
                events: VecDeque::with_capacity(FRAME_CHANNEL_CAPACITY),
                pending_exit: None,
                closed: false,
            }),
            notify: Notify::new(),
        })
    }

    pub async fn recv(&self) -> Option<SessionEvent> {
        loop {
            {
                let mut inner = self.inner.lock().unwrap();
                if let Some(event) = inner.events.pop_front() {
                    return Some(event);
                }
                if let Some(code) = inner.pending_exit.take() {
                    inner.closed = true;
                    return Some(SessionEvent::Exit(code));
                }
                if inner.closed {
                    return None;
                }
            }
            self.notify.notified().await;
        }
    }

    fn push_frame(&self, frame: ScreenFrame) {
        let mut inner = self.inner.lock().unwrap();
        if inner.closed {
            return;
        }
        if inner.events.len() >= FRAME_CHANNEL_CAPACITY {
            inner.events.clear();
        }
        inner.events.push_back(SessionEvent::Frame(frame));
        drop(inner);
        self.notify.notify_one();
    }

    fn try_push_output(&self, output: Vec<u8>) -> bool {
        let mut inner = self.inner.lock().unwrap();
        if inner.closed || inner.events.len() >= FRAME_CHANNEL_CAPACITY {
            return false;
        }
        inner.events.push_back(SessionEvent::Output(output));
        drop(inner);
        self.notify.notify_one();
        true
    }

    fn push_exit_for_mode(&self, code: i32, attach_mode: AttachMode) {
        let mut inner = self.inner.lock().unwrap();
        match attach_mode {
            AttachMode::Frame => {
                inner.events.clear();
                inner.pending_exit = None;
                inner.events.push_back(SessionEvent::Exit(code));
                inner.closed = true;
            }
            AttachMode::Bytes => {
                if !inner.closed && inner.events.len() < FRAME_CHANNEL_CAPACITY {
                    inner.events.push_back(SessionEvent::Exit(code));
                } else {
                    inner.pending_exit = Some(code);
                }
                inner.closed = true;
            }
        }
        drop(inner);
        self.notify.notify_one();
    }

    fn close_when_drained(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.closed = true;
        drop(inner);
        self.notify.notify_one();
    }

    #[cfg(test)]
    pub(crate) fn try_recv(&self) -> Option<SessionEvent> {
        let mut inner = self.inner.lock().unwrap();
        if let Some(event) = inner.events.pop_front() {
            return Some(event);
        }
        if let Some(code) = inner.pending_exit.take() {
            inner.closed = true;
            return Some(SessionEvent::Exit(code));
        }
        None
    }
}

/// 256KB ring scrollback buffer (byte-level)
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
    pub cwd: String,
    pub created_at: DateTime<Utc>,
    width: Arc<Mutex<u16>>,
    height: Arc<Mutex<u16>>,
    pub exited: Arc<AtomicBool>,
    pub exit_code: Arc<Mutex<Option<i32>>>,
    pub closed: Arc<AtomicBool>,
    exit_tx: watch::Sender<bool>,
    /// PTY master: used only for resize (after take_writer, writes go through pty_writer)
    pty_master: Arc<Mutex<Option<Box<dyn MasterPty + Send>>>>,
    /// PTY writer: writes input
    pty_writer: Arc<Mutex<Option<Box<dyn Write + Send>>>>,
    child_killer: Arc<Mutex<Option<Box<dyn ChildKiller + Send + Sync>>>>,
    cursor_shape: Arc<Mutex<u8>>,
    pub scrollback: Arc<Mutex<ScrollbackBuf>>,
    screen: Arc<Mutex<vt100::Parser>>,
    event_lock: Arc<Mutex<()>>,
    /// Attached clients; empty map = detached
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
        Self::new_with_cwd(session_id, name, width, height, shell, None)
    }

    pub fn new_with_cwd(
        session_id: String,
        name: String,
        width: u32,
        height: u32,
        shell: Option<&str>,
        cwd: Option<&Path>,
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

        // Grab the reader and writer first, then wrap master into Arc<Mutex>
        let pty_reader = pair.master.try_clone_reader().context("try_clone_reader")?;
        let pty_writer = pair.master.take_writer().context("take_writer")?;

        let mut cmd = default_shell_command(shell).context("resolve shell command")?;
        apply_working_directory(&mut cmd, cwd)?;
        let resolved_cwd = resolve_session_cwd(cwd);
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
        let (exit_tx, _) = watch::channel(false);
        let pty_master = Arc::new(Mutex::new(Some(pair.master)));
        let pty_writer_arc = Arc::new(Mutex::new(Some(pty_writer)));
        let cursor_shape = Arc::new(Mutex::new(CURSOR_SHAPE_DEFAULT));
        let screen = Arc::new(Mutex::new(vt100::Parser::new(h, w, 0)));
        let event_lock = Arc::new(Mutex::new(()));

        let sess = Arc::new(Session {
            session_id,
            name: Arc::new(Mutex::new(name.clone())),
            cwd: resolved_cwd,
            created_at: Utc::now(),
            width: Arc::new(Mutex::new(w)),
            height: Arc::new(Mutex::new(h)),
            exited: exited.clone(),
            exit_code: exit_code.clone(),
            closed: closed.clone(),
            exit_tx: exit_tx.clone(),
            pty_master: pty_master.clone(),
            pty_writer: pty_writer_arc.clone(),
            child_killer: child_killer.clone(),
            cursor_shape: cursor_shape.clone(),
            scrollback: scrollback.clone(),
            screen: screen.clone(),
            event_lock: event_lock.clone(),
            attached_clients: attached_clients.clone(),
            active_client_id: active_client_id.clone(),
            next_client_id,
        });

        // PTY output reader task: coalesce first, then atomically update the parser to avoid reattach capturing a half frame.
        {
            spawn_coalesced_reader(
                pty_reader,
                OutputReaderState {
                    scrollback: scrollback.clone(),
                    screen: screen.clone(),
                    cursor_shape: cursor_shape.clone(),
                    event_lock: event_lock.clone(),
                    attached_clients: attached_clients.clone(),
                    active_client_id: active_client_id.clone(),
                    exited: exited.clone(),
                    name: name.clone(),
                },
            );
        }

        // Child process exit wait task
        {
            let attached_e = attached_clients.clone();
            let active_client_e = active_client_id.clone();
            let exited_e = exited.clone();
            let exit_code_e = exit_code.clone();
            let exit_tx_e = exit_tx.clone();
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
                    client.queue.push_exit_for_mode(code, client.attach_mode);
                }
                *active_client_e.lock().unwrap() = None;
                let _ = exit_tx_e.send(true);
            });
        }

        Ok(sess)
    }

    pub fn name(&self) -> String {
        self.name.lock().unwrap().clone()
    }

    pub fn subscribe_exit(&self) -> watch::Receiver<bool> {
        self.exit_tx.subscribe()
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
        self.screen
            .lock()
            .unwrap()
            .screen_mut()
            .set_size(height, width);
        Ok(())
    }

    fn client_size(&self, client_id: ClientId) -> anyhow::Result<(u16, u16)> {
        let attached = self.attached_clients.lock().unwrap();
        let client = attached
            .get(&client_id)
            .ok_or_else(|| anyhow::anyhow!("client {client_id} is not attached"))?;
        Ok((client.width, client.height))
    }

    /// Write input to the PTY (forwarded from client Input command)
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

    pub fn scrollback_snapshot(&self) -> Vec<u8> {
        self.scrollback.lock().unwrap().snapshot()
    }

    /// Attach a client: register the event sender and return the initial event (frame or scrollback bytes)
    /// Returns Err if the session has already exited
    pub fn attach(
        &self,
        queue: Arc<SessionEventQueue>,
        width: u32,
        height: u32,
        attach_mode: AttachMode,
    ) -> anyhow::Result<(ClientId, Option<SessionEvent>)> {
        if self.exited.load(Ordering::SeqCst) {
            anyhow::bail!("session has exited");
        }
        if self.closed.load(Ordering::SeqCst) {
            anyhow::bail!("session is closed");
        }
        let _event_guard = self.event_lock.lock().unwrap();
        self.resize(width, height)?;

        let initial_event = match attach_mode {
            AttachMode::Frame => Some(SessionEvent::Frame(self.screen_frame())),
            AttachMode::Bytes => {
                let snapshot = self.scrollback.lock().unwrap().snapshot();
                if snapshot.is_empty() {
                    None
                } else {
                    Some(SessionEvent::Output(snapshot))
                }
            }
        };
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
                queue,
                width: w,
                height: h,
                attach_mode,
                attached: true,
            },
        );
        *self.active_client_id.lock().unwrap() = Some(client_id);
        Ok((client_id, initial_event))
    }

    fn screen_frame(&self) -> ScreenFrame {
        let parser = self.screen.lock().unwrap();
        screen_frame_from_parser(&parser, *self.cursor_shape.lock().unwrap())
    }

    pub fn focus_client(&self, client_id: ClientId) -> anyhow::Result<()> {
        let (width, height) = self.client_size(client_id)?;
        self.mark_client_attached(client_id)?;
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
        let was_detached = {
            let mut attached = self.attached_clients.lock().unwrap();
            let client = attached
                .get_mut(&client_id)
                .ok_or_else(|| anyhow::anyhow!("client {client_id} is not attached"))?;
            let was_detached = !client.attached;
            client.width = width as u16;
            client.height = height as u16;
            client.attached = true;
            was_detached
        };
        if was_detached {
            *self.active_client_id.lock().unwrap() = Some(client_id);
        }
        if *self.active_client_id.lock().unwrap() == Some(client_id) {
            self.resize_pty(width as u16, height as u16)?;
        }
        Ok(())
    }

    fn mark_client_attached(&self, client_id: ClientId) -> anyhow::Result<()> {
        let mut attached = self.attached_clients.lock().unwrap();
        let client = attached
            .get_mut(&client_id)
            .ok_or_else(|| anyhow::anyhow!("client {client_id} is not attached"))?;
        client.attached = true;
        Ok(())
    }

    /// Detach the specified client (idempotent)
    pub fn detach(&self, client_id: ClientId) {
        let mut attached = self.attached_clients.lock().unwrap();
        if let Some(client) = attached.get_mut(&client_id) {
            if client.attach_mode == AttachMode::Bytes {
                client.attached = false;
            } else {
                attached.remove(&client_id);
            }
        }
        drop(attached);
        if *self.active_client_id.lock().unwrap() == Some(client_id) {
            *self.active_client_id.lock().unwrap() = None;
        }
    }

    /// Disconnect the specified client connection (idempotent)
    pub fn disconnect(&self, client_id: ClientId) {
        self.attached_clients.lock().unwrap().remove(&client_id);
        if *self.active_client_id.lock().unwrap() == Some(client_id) {
            *self.active_client_id.lock().unwrap() = None;
        }
    }

    /// Close the session (kill PTY)
    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        if let Some(mut killer) = self.child_killer.lock().unwrap().take() {
            let _ = killer.kill();
        }
        // Drop writer and master → PTY pipe closes → reader task ends
        self.pty_writer.lock().unwrap().take();
        self.pty_master.lock().unwrap().take();
        // Notify the attached clients
        let mut attached = self.attached_clients.lock().unwrap();
        for (_, client) in attached.drain() {
            client.queue.push_exit_for_mode(-1, client.attach_mode);
        }
        *self.active_client_id.lock().unwrap() = None;
    }

    pub fn is_attached(&self) -> bool {
        self.attached_clients
            .lock()
            .unwrap()
            .values()
            .any(|client| client.attached)
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

fn screen_frame_from_parser(parser: &vt100::Parser, cursor_shape: u8) -> ScreenFrame {
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
                cell.contents().to_string()
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
        alternate_screen: screen.alternate_screen() || last_row_has_content(screen, rows, cols),
        cursor_shape,
        application_cursor: screen.application_cursor(),
        bracketed_paste: screen.bracketed_paste(),
        mouse_mode: frame_mouse_mode(screen.mouse_protocol_mode()),
        mouse_encoding: frame_mouse_encoding(screen.mouse_protocol_encoding()),
        rows_v2,
    }
}

fn frame_mouse_mode(mode: vt100::MouseProtocolMode) -> FrameMouseMode {
    match mode {
        vt100::MouseProtocolMode::None => FrameMouseMode::None,
        vt100::MouseProtocolMode::Press => FrameMouseMode::Press,
        vt100::MouseProtocolMode::PressRelease => FrameMouseMode::PressRelease,
        vt100::MouseProtocolMode::ButtonMotion => FrameMouseMode::ButtonMotion,
        vt100::MouseProtocolMode::AnyMotion => FrameMouseMode::AnyMotion,
    }
}

fn frame_mouse_encoding(encoding: vt100::MouseProtocolEncoding) -> FrameMouseEncoding {
    match encoding {
        vt100::MouseProtocolEncoding::Default => FrameMouseEncoding::Default,
        vt100::MouseProtocolEncoding::Utf8 => FrameMouseEncoding::Utf8,
        vt100::MouseProtocolEncoding::Sgr => FrameMouseEncoding::Sgr,
    }
}

fn cleanup_alt_screen_frame(
    parser: &mut vt100::Parser,
    cursor_shape: &Arc<Mutex<u8>>,
) -> Option<ScreenFrame> {
    if !parser.screen().alternate_screen() {
        return None;
    }
    parser.process(b"\x1b[?25h\x1b[?1049l");
    *cursor_shape.lock().unwrap() = CURSOR_SHAPE_DEFAULT;
    Some(screen_frame_from_parser(parser, CURSOR_SHAPE_DEFAULT))
}

fn scan_cursor_shape(data: &[u8]) -> Option<u8> {
    let mut last_shape = None;
    let mut i = 0;
    while i < data.len() {
        if data[i] == 0x1b && i + 1 < data.len() && data[i + 1] == b'[' {
            let mut j = i + 2;
            let mut param = 0u8;
            while j < data.len() && data[j].is_ascii_digit() {
                param = param.saturating_mul(10).saturating_add(data[j] - b'0');
                j += 1;
            }
            if j + 1 < data.len() && data[j] == b' ' && data[j + 1] == b'q' {
                if param <= 6 {
                    last_shape = Some(param);
                }
                i = j + 2;
                continue;
            }
        }
        i += 1;
    }
    last_shape
}

fn scan_rmcup(data: &[u8]) -> bool {
    const RMCUP: &[u8] = b"\x1b[?1049l";
    data.windows(RMCUP.len()).any(|window| window == RMCUP)
}

fn default_run_attrs() -> (FrameColor, FrameColor, u8) {
    (FrameColor::Default, FrameColor::Default, 0)
}

fn cell_run_attrs(cell: &vt100::Cell) -> (FrameColor, FrameColor, u8) {
    let mut flags = 0;
    if cell.dim() {
        flags |= FRAME_FLAG_DIM;
    }
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
    if cell.blink() {
        flags |= FRAME_FLAG_BLINK;
    }
    if cell.hidden() {
        flags |= FRAME_FLAG_HIDDEN;
    }
    if cell.strikethrough() {
        flags |= FRAME_FLAG_STRIKETHROUGH;
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

fn last_row_has_content(screen: &vt100::Screen, rows: u16, cols: u16) -> bool {
    let row = rows.saturating_sub(1);
    for col in 0..cols {
        if let Some(cell) = screen.cell(row, col) {
            let text = cell.contents();
            if !text.is_empty() && text != " " {
                return true;
            }
        }
    }
    false
}

struct OutputReaderState {
    scrollback: Arc<Mutex<ScrollbackBuf>>,
    screen: Arc<Mutex<vt100::Parser>>,
    cursor_shape: Arc<Mutex<u8>>,
    event_lock: Arc<Mutex<()>>,
    attached_clients: Arc<Mutex<HashMap<ClientId, AttachedClient>>>,
    active_client_id: Arc<Mutex<Option<ClientId>>>,
    exited: Arc<AtomicBool>,
    name: String,
}

fn spawn_coalesced_reader(mut reader: Box<dyn Read + Send>, state: OutputReaderState) {
    const COALESCE_TICK_MS: u64 = 1;
    const COALESCE_MAX_MS: u128 = 8;

    let OutputReaderState {
        scrollback,
        screen,
        cursor_shape,
        event_lock,
        attached_clients,
        active_client_id,
        exited,
        name,
    } = state;

    let staging = Arc::new((
        Mutex::new(Vec::<u8>::with_capacity(128 * 1024)),
        Condvar::new(),
    ));
    let reader_done = Arc::new(AtomicBool::new(false));

    {
        let staging_r = staging.clone();
        let reader_done_r = reader_done.clone();
        thread::spawn(move || {
            let mut local = vec![0u8; 64 * 1024];
            loop {
                match reader.read(&mut local) {
                    Ok(n) if n > 0 => {
                        let (lock, cv) = &*staging_r;
                        if let Ok(mut buf) = lock.lock() {
                            buf.extend_from_slice(&local[..n]);
                            cv.notify_one();
                        }
                    }
                    Ok(_) | Err(_) => break,
                }
            }
            reader_done_r.store(true, Ordering::Release);
            let (_, cv) = &*staging_r;
            cv.notify_all();
        });
    }

    thread::spawn(move || {
        loop {
            {
                let (lock, cv) = &*staging;
                let mut buf = match lock.lock() {
                    Ok(guard) => guard,
                    Err(_) => break,
                };
                while buf.is_empty() {
                    if reader_done.load(Ordering::Acquire) {
                        tracing::debug!(session = %name, "pty reader ended");
                        let cleanup_frame = {
                            let _event_guard = event_lock.lock().unwrap();
                            let mut parser = screen.lock().unwrap();
                            cleanup_alt_screen_frame(&mut parser, &cursor_shape)
                        };
                        if let Some(frame) = cleanup_frame {
                            broadcast_frame(&attached_clients, &active_client_id, frame);
                        }
                        exited.store(true, Ordering::SeqCst);
                        return;
                    }
                    match cv.wait_timeout(buf, Duration::from_millis(100)) {
                        Ok((guard, _)) => buf = guard,
                        Err(_) => return,
                    }
                }
            }

            let start = Instant::now();
            let mut last_len = staging.0.lock().map(|buf| buf.len()).unwrap_or(0);
            loop {
                if start.elapsed().as_millis() >= COALESCE_MAX_MS {
                    break;
                }
                thread::sleep(Duration::from_millis(COALESCE_TICK_MS));
                let current_len = staging.0.lock().map(|buf| buf.len()).unwrap_or(0);
                if current_len == last_len {
                    break;
                }
                last_len = current_len;
            }

            let bytes = match staging.0.lock() {
                Ok(mut buf) => std::mem::take(&mut *buf),
                Err(_) => break,
            };
            if bytes.is_empty() {
                continue;
            }

            let _event_guard = event_lock.lock().unwrap();
            scrollback.lock().unwrap().append(&bytes);
            let current_cursor_shape = {
                let mut cursor_shape = cursor_shape.lock().unwrap();
                if let Some(shape) = scan_cursor_shape(&bytes) {
                    *cursor_shape = shape;
                }
                if scan_rmcup(&bytes) {
                    *cursor_shape = CURSOR_SHAPE_DEFAULT;
                }
                *cursor_shape
            };
            let frame = {
                let mut parser = screen.lock().unwrap();
                parser.process(&bytes);
                screen_frame_from_parser(&parser, current_cursor_shape)
            };
            broadcast_output_or_frame(&attached_clients, &active_client_id, &bytes, frame);
        }

        tracing::debug!(session = %name, "pty parser ended");
        exited.store(true, Ordering::SeqCst);
    });
}

fn broadcast_frame(
    attached_clients: &Arc<Mutex<HashMap<ClientId, AttachedClient>>>,
    _active_client_id: &Arc<Mutex<Option<ClientId>>>,
    frame: ScreenFrame,
) {
    let attached = attached_clients.lock().unwrap();
    for client in attached.values() {
        if client.attach_mode == AttachMode::Frame {
            client.queue.push_frame(frame.clone());
        }
    }
}

pub(crate) fn output_chunks(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    data.chunks(MAX_PAYLOAD_SIZE)
}

fn broadcast_output_or_frame(
    attached_clients: &Arc<Mutex<HashMap<ClientId, AttachedClient>>>,
    active_client_id: &Arc<Mutex<Option<ClientId>>>,
    output: &[u8],
    frame: ScreenFrame,
) {
    let mut overflowed_clients = Vec::new();
    {
        let attached = attached_clients.lock().unwrap();
        for (&client_id, client) in attached.iter() {
            match client.attach_mode {
                AttachMode::Frame => client.queue.push_frame(frame.clone()),
                AttachMode::Bytes => {
                    for chunk in output_chunks(output) {
                        if !client.queue.try_push_output(chunk.to_vec()) {
                            overflowed_clients.push(client_id);
                            break;
                        }
                    }
                }
            }
        }
    }

    if overflowed_clients.is_empty() {
        return;
    }

    let mut attached = attached_clients.lock().unwrap();
    let mut removed_active = false;
    for client_id in overflowed_clients {
        if let Some(client) = attached.remove(&client_id) {
            client.queue.close_when_drained();
            removed_active |= *active_client_id.lock().unwrap() == Some(client_id);
            tracing::debug!(client_id, "byte attach queue overflow; client removed");
        }
    }
    if removed_active {
        *active_client_id.lock().unwrap() = None;
    }
}

#[cfg(any(windows, test))]
fn resolve_windows_shell_preference(preference: Option<&str>) -> anyhow::Result<String> {
    let Some(value) = preference.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(DEFAULT_WINDOWS_SHELL.to_string());
    };
    match value {
        "cmd" | "cmd.exe" => return Ok(CMD_WINDOWS_SHELL.to_string()),
        "powershell" | "powershell.exe" => return Ok(DEFAULT_WINDOWS_SHELL.to_string()),
        _ => {}
    }
    // Any other value is treated as a path to a shell executable. When it looks
    // like a path (contains a separator), require that the file exists so typos
    // fail fast with a clear message; bare command names are passed through for
    // PATH resolution, mirroring the Unix branch.
    if value.contains('\\') || value.contains('/') {
        let metadata = std::fs::metadata(value)
            .with_context(|| format!("Windows shell executable not found: {value}"))?;
        if !metadata.is_file() {
            anyhow::bail!("Windows shell path is not a file: {value}");
        }
    }
    Ok(value.to_string())
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

fn apply_working_directory(cmd: &mut CommandBuilder, cwd: Option<&Path>) -> anyhow::Result<()> {
    let Some(cwd) = cwd.filter(|value| !value.as_os_str().is_empty()) else {
        return Ok(());
    };
    let metadata = std::fs::metadata(cwd)
        .with_context(|| format!("working directory does not exist: {}", cwd.display()))?;
    if !metadata.is_dir() {
        anyhow::bail!("working directory is not a directory: {}", cwd.display());
    }
    cmd.cwd(cwd);
    Ok(())
}

/// Return the working directory the session actually uses: use the explicitly specified one if given, otherwise fall back to the daemon's current directory.
fn resolve_session_cwd(cwd: Option<&Path>) -> String {
    if let Some(cwd) = cwd.filter(|value| !value.as_os_str().is_empty()) {
        return cwd.to_string_lossy().into_owned();
    }
    std::env::current_dir()
        .ok()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn working_directory_uses_non_empty_request_value() {
        let mut cmd = default_shell_command(None).unwrap();
        let cwd = std::env::current_dir().unwrap();
        apply_working_directory(&mut cmd, Some(cwd.as_path())).unwrap();

        assert_eq!(
            cmd.get_cwd().map(|value| value.as_os_str()),
            Some(cwd.as_os_str())
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn working_directory_preserves_non_utf8_path() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let mut name = format!("qscreen-cwd-{}-", std::process::id()).into_bytes();
        name.push(0xff);
        let cwd = std::env::temp_dir().join(OsString::from_vec(name));
        std::fs::create_dir(&cwd).unwrap();
        let mut cmd = default_shell_command(None).unwrap();

        apply_working_directory(&mut cmd, Some(&cwd)).unwrap();

        assert_eq!(
            cmd.get_cwd().map(OsString::as_os_str),
            Some(cwd.as_os_str())
        );
        std::fs::remove_dir(cwd).unwrap();
    }

    #[test]
    fn working_directory_ignores_empty_request_value() {
        let mut cmd = default_shell_command(None).unwrap();
        apply_working_directory(&mut cmd, Some(Path::new(""))).unwrap();

        assert_eq!(cmd.get_cwd(), None);
    }

    #[test]
    fn working_directory_rejects_missing_path() {
        let mut cmd = default_shell_command(None).unwrap();
        let missing =
            std::env::temp_dir().join(format!("qscreen-missing-cwd-{}", std::process::id()));

        let err = apply_working_directory(&mut cmd, Some(missing.as_path())).unwrap_err();

        assert!(err.to_string().contains("does not exist"), "{err:#}");
    }

    #[test]
    fn working_directory_rejects_file() {
        let mut cmd = default_shell_command(None).unwrap();
        let file = std::env::temp_dir().join(format!("qscreen-file-cwd-{}", std::process::id()));
        std::fs::write(&file, b"not a directory").unwrap();

        let err = apply_working_directory(&mut cmd, Some(file.as_path())).unwrap_err();
        let _ = std::fs::remove_file(file);

        assert!(err.to_string().contains("not a directory"), "{err:#}");
    }

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
    fn windows_shell_preference_passes_through_bare_command() {
        // Bare command names (no path separator) are forwarded for PATH
        // resolution, e.g. `pwsh` for PowerShell 7.
        assert_eq!(
            resolve_windows_shell_preference(Some("pwsh")).unwrap(),
            "pwsh"
        );
    }

    #[test]
    fn windows_shell_preference_accepts_existing_executable_path() {
        let exe = std::env::current_exe().expect("current exe path");
        let path = exe.to_str().expect("exe path is utf-8");
        assert_eq!(
            resolve_windows_shell_preference(Some(path)).unwrap(),
            path.to_string()
        );
    }

    #[test]
    fn windows_shell_preference_rejects_missing_path() {
        let err = resolve_windows_shell_preference(Some(r"C:\does\not\exist\pwsh.exe"))
            .expect_err("missing path should fail")
            .to_string();

        assert!(err.contains("Windows shell executable not found"), "{err}");
    }

    #[test]
    fn windows_shell_preference_rejects_directory_path() {
        let dir = std::env::temp_dir();
        let path = dir.to_str().expect("temp dir path is utf-8");
        let err = resolve_windows_shell_preference(Some(path))
            .expect_err("directory should fail")
            .to_string();

        assert!(err.contains("is not a file"), "{err}");
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

    fn recv_frame_matching(queue: &SessionEventQueue, expected: &str) {
        for _ in 0..16 {
            match queue.try_recv().expect("expected session event") {
                SessionEvent::Frame(frame)
                    if frame
                        .rows_v2
                        .iter()
                        .flat_map(|row| row.iter())
                        .any(|run| run.text.contains(expected)) =>
                {
                    return;
                }
                SessionEvent::Frame(_) => {}
                SessionEvent::Output(output) => {
                    panic!(
                        "expected frame, got output {}",
                        String::from_utf8_lossy(&output)
                    )
                }
                SessionEvent::Exit(code) => panic!("expected frame, got exit {code}"),
            }
        }
        panic!("expected matching frame");
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

    fn expect_frame(event: Option<SessionEvent>) -> ScreenFrame {
        match event.expect("expected initial event") {
            SessionEvent::Frame(frame) => frame,
            SessionEvent::Output(output) => {
                panic!(
                    "expected frame, got output {}",
                    String::from_utf8_lossy(&output)
                )
            }
            SessionEvent::Exit(code) => panic!("expected frame, got exit {code}"),
        }
    }

    #[tokio::test]
    async fn attach_multiple_clients_and_detach_independently() {
        let session =
            Session::new("1".to_string(), "multi-detach".to_string(), 80, 24, None).unwrap();
        let queue1 = SessionEventQueue::new();
        let queue2 = SessionEventQueue::new();

        let (client1, frame1) = session.attach(queue1, 80, 24, AttachMode::Frame).unwrap();
        let (client2, frame2) = session.attach(queue2, 100, 30, AttachMode::Frame).unwrap();
        let frame1 = expect_frame(frame1);
        let frame2 = expect_frame(frame2);

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
    async fn broadcast_frame_reaches_all_clients() {
        let session =
            Session::new("1".to_string(), "multi-broadcast".to_string(), 80, 24, None).unwrap();
        let queue1 = SessionEventQueue::new();
        let queue2 = SessionEventQueue::new();
        let queue3 = SessionEventQueue::new();

        let (client1, _) = session
            .attach(queue1.clone(), 80, 24, AttachMode::Frame)
            .unwrap();
        let (_middle_client, _) = session.attach(queue2, 80, 24, AttachMode::Frame).unwrap();
        let (client3, _) = session
            .attach(queue3.clone(), 80, 24, AttachMode::Frame)
            .unwrap();

        let frame = ScreenFrame {
            rows: 1,
            cols: 5,
            rows_v2: vec![vec![ScreenRun {
                text: "chunk".to_string(),
                fg: FrameColor::Default,
                bg: FrameColor::Default,
                flags: 0,
                width: 5,
            }]],
            ..Default::default()
        };
        broadcast_frame(&session.attached_clients, &session.active_client_id, frame);

        recv_frame_matching(&queue1, "chunk");
        recv_frame_matching(&queue3, "chunk");
        let attached = session.attached_clients.lock().unwrap();
        assert!(attached.contains_key(&client1));
        assert!(attached.contains_key(&client3));
        drop(attached);

        session.close();
    }

    #[test]
    fn event_queue_drops_old_frames_when_full() {
        let queue = SessionEventQueue::new();
        for idx in 0..(FRAME_CHANNEL_CAPACITY + 2) {
            queue.push_frame(ScreenFrame {
                rows: 1,
                cols: 1,
                rows_v2: vec![vec![ScreenRun {
                    text: idx.to_string(),
                    fg: FrameColor::Default,
                    bg: FrameColor::Default,
                    flags: 0,
                    width: 1,
                }]],
                ..Default::default()
            });
        }

        let first = match queue.try_recv().unwrap() {
            SessionEvent::Frame(frame) => frame.rows_v2[0][0].text.clone(),
            SessionEvent::Output(output) => {
                panic!(
                    "expected frame, got output {}",
                    String::from_utf8_lossy(&output)
                )
            }
            SessionEvent::Exit(code) => panic!("expected frame, got exit {code}"),
        };

        assert_eq!(first, FRAME_CHANNEL_CAPACITY.to_string());
    }

    #[test]
    fn cleanup_alt_screen_frame_exits_alternate_screen() {
        let mut parser = vt100::Parser::new(2, 8, 0);
        let cursor_shape = Arc::new(Mutex::new(5));
        parser.process(b"main\x1b[?1049halt");
        assert!(parser.screen().alternate_screen());

        let frame =
            cleanup_alt_screen_frame(&mut parser, &cursor_shape).expect("expected cleanup frame");

        assert!(!parser.screen().alternate_screen());
        assert!(!frame.alternate_screen);
        assert_eq!(frame.cursor_shape, CURSOR_SHAPE_DEFAULT);
        assert_eq!(*cursor_shape.lock().unwrap(), CURSOR_SHAPE_DEFAULT);
    }

    #[test]
    fn scan_cursor_shape_returns_last_valid_shape() {
        assert_eq!(scan_cursor_shape(b"\x1b[2 qtext\x1b[5 q"), Some(5));
        assert_eq!(scan_cursor_shape(b"\x1b[9 q"), None);
        assert_eq!(scan_cursor_shape(b"plain"), None);
    }

    #[test]
    fn scan_rmcup_detects_alternate_screen_exit() {
        assert!(scan_rmcup(b"before\x1b[?1049lafter"));
        assert!(!scan_rmcup(b"\x1b[?1049h"));
    }

    #[tokio::test]
    async fn close_notifies_all_attached_clients() {
        let session =
            Session::new("1".to_string(), "multi-close".to_string(), 80, 24, None).unwrap();
        let queue1 = SessionEventQueue::new();
        let queue2 = SessionEventQueue::new();

        let _ = session
            .attach(queue1.clone(), 80, 24, AttachMode::Frame)
            .unwrap();
        let _ = session
            .attach(queue2.clone(), 80, 24, AttachMode::Frame)
            .unwrap();

        session.close();

        assert_eq!(recv_exit(&queue1), -1);
        assert_eq!(recv_exit(&queue2), -1);
        assert!(!session.is_attached());
        assert_eq!(*session.active_client_id.lock().unwrap(), None);
    }

    #[tokio::test]
    async fn inactive_resize_stores_size_without_pty_resize_until_focus_or_input() {
        let session =
            Session::new("1".to_string(), "multi-size".to_string(), 80, 24, None).unwrap();
        let queue1 = SessionEventQueue::new();
        let queue2 = SessionEventQueue::new();

        let (client1, _) = session.attach(queue1, 80, 24, AttachMode::Frame).unwrap();
        let (client2, _) = session.attach(queue2, 100, 30, AttachMode::Frame).unwrap();

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
        let queue1 = SessionEventQueue::new();
        let queue2 = SessionEventQueue::new();

        let (client1, _) = session.attach(queue1, 80, 24, AttachMode::Frame).unwrap();
        let (client2, _) = session.attach(queue2, 100, 30, AttachMode::Frame).unwrap();
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
            screen.process(b"\x1b[?1h\x1b[?1000h\x1b[?1006h\x1b[?2004h\x1b[2J\x1b[Hcurrent");
        }
        let queue = SessionEventQueue::new();

        let (_, frame) = session.attach(queue, 80, 24, AttachMode::Frame).unwrap();
        let frame = expect_frame(frame);
        let frame_text = frame
            .rows_v2
            .iter()
            .flat_map(|row| row.iter())
            .map(|run| run.text.as_str())
            .collect::<String>();

        assert_eq!((frame.cols, frame.rows), (80, 24));
        assert!(frame_text.contains("current"));
        assert!(!frame_text.contains("old history"));
        assert!(frame.application_cursor);
        assert!(frame.bracketed_paste);
        assert_eq!(frame.mouse_mode, FrameMouseMode::PressRelease);
        assert_eq!(frame.mouse_encoding, FrameMouseEncoding::Sgr);

        session.close();
    }

    #[test]
    fn output_chunks_split_over_limit_payload() {
        let data = vec![b'x'; MAX_PAYLOAD_SIZE * 2 + 3];
        let chunks = output_chunks(&data)
            .map(|chunk| chunk.len())
            .collect::<Vec<_>>();

        assert_eq!(chunks, vec![MAX_PAYLOAD_SIZE, MAX_PAYLOAD_SIZE, 3]);
    }

    #[test]
    fn mixed_frame_bytes_client_behavior() {
        let attached_clients = Arc::new(Mutex::new(HashMap::new()));
        let active_client_id = Arc::new(Mutex::new(None));
        let frame_queue = SessionEventQueue::new();
        let bytes_queue = SessionEventQueue::new();
        attached_clients.lock().unwrap().insert(
            1,
            AttachedClient {
                queue: frame_queue.clone(),
                width: 80,
                height: 24,
                attach_mode: AttachMode::Frame,
                attached: true,
            },
        );
        attached_clients.lock().unwrap().insert(
            2,
            AttachedClient {
                queue: bytes_queue.clone(),
                width: 80,
                height: 24,
                attach_mode: AttachMode::Bytes,
                attached: true,
            },
        );

        let frame = ScreenFrame {
            rows: 1,
            cols: 4,
            rows_v2: vec![vec![ScreenRun {
                text: "live".to_string(),
                fg: FrameColor::Default,
                bg: FrameColor::Default,
                flags: 0,
                width: 4,
            }]],
            ..Default::default()
        };
        broadcast_output_or_frame(&attached_clients, &active_client_id, b"\x1b[31mlive", frame);

        match frame_queue.try_recv().unwrap() {
            SessionEvent::Frame(frame) => assert_eq!(frame.rows_v2[0][0].text, "live"),
            SessionEvent::Output(_) => panic!("frame client received output"),
            SessionEvent::Exit(code) => panic!("expected frame, got exit {code}"),
        }
        match bytes_queue.try_recv().unwrap() {
            SessionEvent::Output(output) => assert_eq!(output, b"\x1b[31mlive"),
            SessionEvent::Frame(_) => panic!("bytes client received frame"),
            SessionEvent::Exit(code) => panic!("expected output, got exit {code}"),
        }
    }

    #[tokio::test]
    async fn byte_detach_keeps_observer_stream_and_reattaches_on_resize() {
        let session =
            Session::new("1".to_string(), "byte-detach".to_string(), 80, 24, None).unwrap();
        let queue = SessionEventQueue::new();
        let (client_id, _) = session
            .attach(queue.clone(), 80, 24, AttachMode::Bytes)
            .unwrap();

        assert!(session.is_attached());
        session.detach(client_id);
        assert!(!session.is_attached());
        {
            let attached = session.attached_clients.lock().unwrap();
            let client = attached.get(&client_id).expect("byte observer kept");
            assert_eq!(client.attach_mode, AttachMode::Bytes);
            assert!(!client.attached);
        }

        let frame = ScreenFrame::default();
        broadcast_output_or_frame(
            &session.attached_clients,
            &session.active_client_id,
            b"live",
            frame,
        );
        match queue.try_recv().unwrap() {
            SessionEvent::Output(output) => assert_eq!(output, b"live"),
            SessionEvent::Frame(_) => panic!("expected output, got frame"),
            SessionEvent::Exit(code) => panic!("expected output, got exit {code}"),
        }

        session.resize_client(client_id, 100, 30).unwrap();
        assert!(session.is_attached());
        assert_eq!(*session.active_client_id.lock().unwrap(), Some(client_id));
        assert_eq!((session.width(), session.height()), (100, 30));

        session.close();
    }

    #[test]
    fn byte_queue_overflow_disconnects_and_removes_client() {
        let attached_clients = Arc::new(Mutex::new(HashMap::new()));
        let active_client_id = Arc::new(Mutex::new(Some(1)));
        let bytes_queue = SessionEventQueue::new();
        attached_clients.lock().unwrap().insert(
            1,
            AttachedClient {
                queue: bytes_queue.clone(),
                width: 80,
                height: 24,
                attach_mode: AttachMode::Bytes,
                attached: true,
            },
        );
        for idx in 0..FRAME_CHANNEL_CAPACITY {
            assert!(bytes_queue.try_push_output(vec![idx as u8]));
        }

        broadcast_output_or_frame(
            &attached_clients,
            &active_client_id,
            b"overflow",
            ScreenFrame::default(),
        );

        assert!(attached_clients.lock().unwrap().is_empty());
        assert_eq!(*active_client_id.lock().unwrap(), None);
        for idx in 0..FRAME_CHANNEL_CAPACITY {
            match bytes_queue.try_recv().unwrap() {
                SessionEvent::Output(output) => assert_eq!(output, vec![idx as u8]),
                SessionEvent::Frame(_) => panic!("expected output, got frame"),
                SessionEvent::Exit(code) => panic!("expected output, got exit {code}"),
            }
        }
        assert!(bytes_queue.try_recv().is_none());
    }

    #[test]
    fn byte_exit_preserves_pending_output_and_appends_exit_when_capacity_available() {
        let queue = SessionEventQueue::new();
        assert!(queue.try_push_output(b"first".to_vec()));
        assert!(queue.try_push_output(b"second".to_vec()));

        queue.push_exit_for_mode(7, AttachMode::Bytes);

        match queue.try_recv().unwrap() {
            SessionEvent::Output(output) => assert_eq!(output, b"first"),
            SessionEvent::Frame(_) => panic!("expected output, got frame"),
            SessionEvent::Exit(code) => panic!("expected output, got exit {code}"),
        }
        match queue.try_recv().unwrap() {
            SessionEvent::Output(output) => assert_eq!(output, b"second"),
            SessionEvent::Frame(_) => panic!("expected output, got frame"),
            SessionEvent::Exit(code) => panic!("expected output, got exit {code}"),
        }
        match queue.try_recv().unwrap() {
            SessionEvent::Exit(code) => assert_eq!(code, 7),
            SessionEvent::Frame(_) => panic!("expected exit, got frame"),
            SessionEvent::Output(output) => {
                panic!(
                    "expected exit, got output {}",
                    String::from_utf8_lossy(&output)
                )
            }
        }
    }

    #[test]
    fn byte_exit_preserves_pending_output_and_exit_when_queue_full() {
        let queue = SessionEventQueue::new();
        for idx in 0..FRAME_CHANNEL_CAPACITY {
            assert!(queue.try_push_output(vec![idx as u8]));
        }

        queue.push_exit_for_mode(7, AttachMode::Bytes);

        for idx in 0..FRAME_CHANNEL_CAPACITY {
            match queue.try_recv().unwrap() {
                SessionEvent::Output(output) => assert_eq!(output, vec![idx as u8]),
                SessionEvent::Frame(_) => panic!("expected output, got frame"),
                SessionEvent::Exit(code) => panic!("expected output, got exit {code}"),
            }
        }
        match queue.try_recv().unwrap() {
            SessionEvent::Exit(code) => assert_eq!(code, 7),
            SessionEvent::Frame(_) => panic!("expected exit, got frame"),
            SessionEvent::Output(output) => {
                panic!(
                    "expected exit, got output {}",
                    String::from_utf8_lossy(&output)
                )
            }
        }
        assert!(queue.try_recv().is_none());
        assert!(!queue.try_push_output(b"after-close".to_vec()));
    }

    #[test]
    fn frame_exit_clears_pending_events_and_keeps_only_exit() {
        let queue = SessionEventQueue::new();
        for idx in 0..3 {
            queue.push_frame(ScreenFrame {
                rows: 1,
                cols: 1,
                rows_v2: vec![vec![ScreenRun {
                    text: idx.to_string(),
                    fg: FrameColor::Default,
                    bg: FrameColor::Default,
                    flags: 0,
                    width: 1,
                }]],
                ..Default::default()
            });
        }

        queue.push_exit_for_mode(9, AttachMode::Frame);

        match queue.try_recv().unwrap() {
            SessionEvent::Exit(code) => assert_eq!(code, 9),
            SessionEvent::Frame(_) => panic!("expected exit, got frame"),
            SessionEvent::Output(output) => {
                panic!(
                    "expected exit, got output {}",
                    String::from_utf8_lossy(&output)
                )
            }
        }
        assert!(queue.try_recv().is_none());
    }
}
