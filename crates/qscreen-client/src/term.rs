use std::io::Write;

const ENABLE_FOCUS_REPORTING: &[u8] = b"\x1b[?1004h";
const RESTORE_AFTER_ATTACH: &[u8] =
    b"\x1b[?2026l\x1b[?2004l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1004l\x1b[?25h\x1b[0m\x1b[r";

pub fn enable_focus_reporting<W: Write>(out: &mut W) -> std::io::Result<()> {
    out.write_all(ENABLE_FOCUS_REPORTING)?;
    out.flush()
}

pub fn cleanup_attach_terminal<W: Write>(out: &mut W) -> std::io::Result<()> {
    let raw_mode_result = crossterm::terminal::disable_raw_mode();
    out.write_all(RESTORE_AFTER_ATTACH)?;
    #[cfg(windows)]
    out.write_all(b"\x1b[?9001l\x1b[!p")?;
    out.flush()?;
    raw_mode_result
}

/// 客户端 VT 状态机 + 渲染器
///
/// 维护与 ConPTY 同步的 grid，从 grid 渲染输出到用户终端，
/// 消除双缓冲 cursor drift（PSReadLine 光标跳跃根因）。
pub struct TermScreen {
    parser: vt100::Parser,
    prev_contents: Vec<u8>,
    rows: u16,
    cols: u16,
}

impl TermScreen {
    pub fn new(rows: u16, cols: u16) -> Self {
        TermScreen {
            parser: vt100::Parser::new(rows, cols, 0),
            prev_contents: Vec::new(),
            rows,
            cols,
        }
    }

    /// 送入 PTY 输出字节，更新内部 grid（不立即渲染）
    pub fn process(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
    }

    /// 将当前 grid 状态渲染到 `out`（全屏 diff，内容未变则跳过）
    ///
    /// 使用 DEC PM 2026（synchronized output）防止闪烁：
    /// Windows Terminal 1.14+ 支持，旧终端忽略该序列无副作用。
    pub fn render<W: Write>(&mut self, out: &mut W) -> std::io::Result<()> {
        let contents = self.parser.screen().contents_formatted();
        if contents == self.prev_contents {
            return Ok(());
        }
        out.write_all(b"\x1b[?2026h")?; // begin synchronized update
        out.write_all(&contents)?;
        out.write_all(b"\x1b[?2026l")?; // end synchronized update
        out.flush()?;
        self.prev_contents = contents;
        Ok(())
    }

    /// 终端 resize 后同步 grid 尺寸（强制下一次全量重绘）
    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.parser.set_size(rows, cols);
        self.prev_contents.clear();
        self.rows = rows;
        self.cols = cols;
    }

    pub fn force_redraw(&mut self) {
        self.prev_contents.clear();
    }

    pub fn size(&self) -> (u16, u16) {
        (self.cols, self.rows)
    }
}
