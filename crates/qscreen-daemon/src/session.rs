use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Context;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use portable_pty::{CommandBuilder, MasterPty, NativePtySystem, PtySize, PtySystem};
use tokio::sync::mpsc;

pub const SCROLLBACK_LIMIT: usize = 256 * 1024;
const DEFAULT_WIDTH: u16 = 80;
const DEFAULT_HEIGHT: u16 = 24;

/// PTY 输出事件，通过 attached_tx 发给当前 attach 的客户端
#[derive(Debug)]
pub enum SessionEvent {
    Output(Bytes),
    Exit(i32),
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
    pub name: String,
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
    pub scrollback: Arc<Mutex<ScrollbackBuf>>,
    /// 已 attach 客户端的事件发送端；None = detached
    pub attached_tx: Arc<Mutex<Option<mpsc::UnboundedSender<SessionEvent>>>>,
}

impl Session {
    pub fn new(name: String, width: u32, height: u32) -> anyhow::Result<Arc<Self>> {
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

        let cmd = default_shell_command();
        let child = pair.slave.spawn_command(cmd).context("spawn shell")?;
        drop(pair.slave);

        let scrollback = Arc::new(Mutex::new(ScrollbackBuf::new()));
        let attached_tx: Arc<Mutex<Option<mpsc::UnboundedSender<SessionEvent>>>> =
            Arc::new(Mutex::new(None));
        let exited = Arc::new(AtomicBool::new(false));
        let exit_code: Arc<Mutex<Option<i32>>> = Arc::new(Mutex::new(None));
        let closed = Arc::new(AtomicBool::new(false));
        let pty_master = Arc::new(Mutex::new(Some(pair.master)));
        let pty_writer_arc = Arc::new(Mutex::new(Some(pty_writer)));

        let sess = Arc::new(Session {
            name: name.clone(),
            created_at: Utc::now(),
            width: Arc::new(Mutex::new(w)),
            height: Arc::new(Mutex::new(h)),
            exited: exited.clone(),
            exit_code: exit_code.clone(),
            closed: closed.clone(),
            pty_master: pty_master.clone(),
            pty_writer: pty_writer_arc.clone(),
            scrollback: scrollback.clone(),
            attached_tx: attached_tx.clone(),
        });

        // PTY output 读取任务（阻塞 IO，放在 spawn_blocking）
        {
            let scrollback_r = scrollback.clone();
            let attached_r = attached_tx.clone();
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
                            if let Some(tx) = attached_r.lock().unwrap().as_ref() {
                                let _ = tx.send(SessionEvent::Output(data));
                            }
                        }
                    }
                }
                tracing::debug!(session = %name_r, "pty reader ended");
                exited_r.store(true, Ordering::SeqCst);
            });
        }

        // 子进程退出等待任务
        {
            let attached_e = attached_tx.clone();
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
                if let Some(tx) = attached_e.lock().unwrap().take() {
                    let _ = tx.send(SessionEvent::Exit(code));
                }
            });
        }

        Ok(sess)
    }

    pub fn width(&self) -> u16 {
        *self.width.lock().unwrap()
    }

    pub fn height(&self) -> u16 {
        *self.height.lock().unwrap()
    }

    /// 写输入到 PTY（forwarded from client Input 命令）
    pub fn write_input(&self, data: &[u8]) -> anyhow::Result<()> {
        if self.closed.load(Ordering::SeqCst) {
            anyhow::bail!("session is closed");
        }
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
        if self.closed.load(Ordering::SeqCst) {
            anyhow::bail!("session is closed");
        }
        let w = width as u16;
        let h = height as u16;
        let guard = self.pty_master.lock().unwrap();
        match guard.as_ref() {
            None => anyhow::bail!("session is closed"),
            Some(m) => {
                m.resize(PtySize {
                    rows: h,
                    cols: w,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .context("resize pty")?;
            }
        }
        drop(guard);
        *self.width.lock().unwrap() = w;
        *self.height.lock().unwrap() = h;
        Ok(())
    }

    /// Attach 一个客户端：返回 scrollback 快照 + 注册事件发送端
    /// 返回 Err 如果已有客户端 attach 或 session 已退出
    pub fn attach(&self, tx: mpsc::UnboundedSender<SessionEvent>) -> anyhow::Result<Vec<u8>> {
        if self.exited.load(Ordering::SeqCst) {
            anyhow::bail!("session has exited");
        }
        if self.closed.load(Ordering::SeqCst) {
            anyhow::bail!("session is closed");
        }
        let mut guard = self.attached_tx.lock().unwrap();
        if guard.is_some() {
            anyhow::bail!("session is already attached");
        }
        let scrollback = self.scrollback.lock().unwrap().snapshot();
        *guard = Some(tx);
        Ok(scrollback)
    }

    /// Detach 当前客户端（幂等）
    pub fn detach(&self) {
        self.attached_tx.lock().unwrap().take();
    }

    /// 关闭 session（kill PTY）
    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        // 丢弃 writer 和 master → PTY 管道关闭 → reader task 结束
        self.pty_writer.lock().unwrap().take();
        self.pty_master.lock().unwrap().take();
        // 通知已 attach 的客户端
        if let Some(tx) = self.attached_tx.lock().unwrap().take() {
            let _ = tx.send(SessionEvent::Exit(-1));
        }
    }

    pub fn is_attached(&self) -> bool {
        self.attached_tx.lock().unwrap().is_some()
    }
}

fn default_shell_command() -> CommandBuilder {
    #[cfg(windows)]
    {
        CommandBuilder::new(r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe")
    }
    #[cfg(unix)]
    {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        let mut cmd = CommandBuilder::new(shell);
        cmd.arg("-l");
        cmd
    }
}
