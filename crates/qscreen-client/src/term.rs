use std::io::Write;

use qscreen_protocol::{
    FRAME_FLAG_BOLD, FRAME_FLAG_INVERSE, FRAME_FLAG_ITALIC, FRAME_FLAG_UNDERLINE, FrameColor,
    ScreenFrame,
};
use unicode_width::UnicodeWidthStr;

const PREPARE_ATTACH: &[u8] = b"\x1b[?1004h\x1b[2J\x1b[H";
const RESTORE_AFTER_ATTACH: &[u8] =
    b"\x1b[?2026l\x1b[?2004l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1004l\x1b[?25h\x1b[0m\x1b[r";

pub fn prepare_attach_terminal<W: Write>(out: &mut W) -> std::io::Result<()> {
    out.write_all(PREPARE_ATTACH)?;
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

pub fn render_screen_frame<W: Write>(out: &mut W, frame: &ScreenFrame) -> std::io::Result<()> {
    if frame.alternate_screen {
        out.write_all(b"\x1b[?1049h")?;
    }
    out.write_all(b"\x1b[?2026h\x1b[?25l\x1b[0m\x1b[2J")?;

    for (row_idx, row) in frame.rows_v2.iter().enumerate().take(frame.rows as usize) {
        write!(out, "\x1b[{};{}H", row_idx + 1, 1)?;
        let mut col: u16 = 0;
        for run in row {
            write_run_attrs(out, run.flags, run.fg, run.bg)?;
            out.write_all(run.text.as_bytes())?;
            let text_width = text_cell_width(&run.text);
            if run.width > text_width {
                write_spaces(out, run.width - text_width)?;
            }
            col = col.saturating_add(run.width);
            if col >= frame.cols {
                break;
            }
        }
        if col < frame.cols {
            out.write_all(b"\x1b[0m")?;
            write_spaces(out, frame.cols - col)?;
        }
    }

    out.write_all(b"\x1b[0m")?;
    write!(
        out,
        "\x1b[{};{}H",
        frame.cursor_row.saturating_add(1),
        frame.cursor_col.saturating_add(1)
    )?;
    if !frame.hide_cursor {
        out.write_all(b"\x1b[?25h")?;
    }
    out.write_all(b"\x1b[?2026l")?;
    out.flush()
}

fn write_run_attrs<W: Write>(
    out: &mut W,
    flags: u8,
    fg: FrameColor,
    bg: FrameColor,
) -> std::io::Result<()> {
    let mut params = vec!["0".to_string()];
    if flags & FRAME_FLAG_BOLD != 0 {
        params.push("1".to_string());
    }
    if flags & FRAME_FLAG_ITALIC != 0 {
        params.push("3".to_string());
    }
    if flags & FRAME_FLAG_UNDERLINE != 0 {
        params.push("4".to_string());
    }
    if flags & FRAME_FLAG_INVERSE != 0 {
        params.push("7".to_string());
    }
    push_color_params(&mut params, true, fg);
    push_color_params(&mut params, false, bg);
    write!(out, "\x1b[{}m", params.join(";"))
}

fn push_color_params(params: &mut Vec<String>, foreground: bool, color: FrameColor) {
    let base = if foreground { "38" } else { "48" };
    match color {
        FrameColor::Default => params.push(if foreground { "39" } else { "49" }.to_string()),
        FrameColor::Idx(value) => {
            params.push(base.to_string());
            params.push("5".to_string());
            params.push(value.to_string());
        }
        FrameColor::Rgb(r, g, b) => {
            params.push(base.to_string());
            params.push("2".to_string());
            params.push(r.to_string());
            params.push(g.to_string());
            params.push(b.to_string());
        }
    }
}

fn text_cell_width(text: &str) -> u16 {
    text.width().min(u16::MAX as usize) as u16
}

fn write_spaces<W: Write>(out: &mut W, count: u16) -> std::io::Result<()> {
    for _ in 0..count {
        out.write_all(b" ")?;
    }
    Ok(())
}

pub struct TermScreen {
    rows: u16,
    cols: u16,
}

impl TermScreen {
    pub fn new(rows: u16, cols: u16) -> Self {
        TermScreen { rows, cols }
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.rows = rows;
        self.cols = cols;
    }

    pub fn size(&self) -> (u16, u16) {
        (self.cols, self.rows)
    }
}
