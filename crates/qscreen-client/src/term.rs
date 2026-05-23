use std::io::Write;

use qscreen_protocol::{
    FRAME_FLAG_BLINK, FRAME_FLAG_BOLD, FRAME_FLAG_DIM, FRAME_FLAG_HIDDEN, FRAME_FLAG_INVERSE,
    FRAME_FLAG_ITALIC, FRAME_FLAG_STRIKETHROUGH, FRAME_FLAG_UNDERLINE, FrameColor, ScreenFrame,
};
use unicode_width::UnicodeWidthStr;

const PREPARE_ATTACH: &[u8] = b"\x1b[?1004h\x1b[2J\x1b[H";
const RESTORE_AFTER_ATTACH: &[u8] =
    b"\x1b[?2026l\x1b[?2004l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1004l\x1b[?25h\x1b[0m\x1b[0 q\x1b[?1049l\x1b[r";

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

#[derive(Default)]
pub struct FrameRenderer {
    previous: Option<ScreenFrame>,
}

impl FrameRenderer {
    pub fn render<W: Write>(&mut self, out: &mut W, frame: &ScreenFrame) -> std::io::Result<()> {
        if self.previous.as_ref() == Some(frame) {
            return Ok(());
        }
        let force_full = self.previous.as_ref().is_none_or(|previous| {
            previous.rows != frame.rows
                || previous.cols != frame.cols
                || previous.alternate_screen != frame.alternate_screen
        });

        render_screen_frame_with_previous(out, frame, self.previous.as_ref(), force_full)?;
        self.previous = Some(frame.clone());
        Ok(())
    }

    pub fn reset(&mut self) {
        self.previous = None;
    }
}

fn render_screen_frame_with_previous<W: Write>(
    out: &mut W,
    frame: &ScreenFrame,
    previous: Option<&ScreenFrame>,
    force_full: bool,
) -> std::io::Result<()> {
    if frame.alternate_screen {
        out.write_all(b"\x1b[?1049h")?;
    }
    out.write_all(b"\x1b[?2026h\x1b[?25l\x1b[0m")?;
    if force_full {
        out.write_all(b"\x1b[2J")?;
    }

    for (row_idx, row) in frame.rows_v2.iter().enumerate().take(frame.rows as usize) {
        if !force_full
            && previous.is_some_and(|previous| previous.rows_v2.get(row_idx) == Some(row))
        {
            continue;
        }
        write!(out, "\x1b[{};{}H", row_idx + 1, 1)?;
        let mut col: u16 = 0;
        let mut last_bg = FrameColor::Default;
        for run in row {
            last_bg = run.bg;
            write_run_attrs(out, run.flags, run.fg, run.bg)?;
            let text = if run.flags & FRAME_FLAG_HIDDEN != 0 {
                " "
            } else {
                run.text.as_str()
            };
            out.write_all(text.as_bytes())?;
            let text_width = text_cell_width(text);
            if run.width > text_width {
                write_spaces(out, run.width - text_width)?;
            }
            col = col.saturating_add(run.width);
            if col >= frame.cols {
                break;
            }
        }
        if col < frame.cols {
            write_run_attrs(out, 0, FrameColor::Default, last_bg)?;
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
    if frame.cursor_shape <= 6
        && previous.is_none_or(|previous| previous.cursor_shape != frame.cursor_shape)
    {
        write!(out, "\x1b[{} q", frame.cursor_shape)?;
    }
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
    if flags & FRAME_FLAG_DIM != 0 {
        params.push("2".to_string());
    }
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
    if flags & FRAME_FLAG_BLINK != 0 {
        params.push("5".to_string());
    }
    if flags & FRAME_FLAG_STRIKETHROUGH != 0 {
        params.push("9".to_string());
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

#[cfg(test)]
mod tests {
    use super::*;
    use qscreen_protocol::ScreenRun;

    fn frame_with_rows(rows: &[&str]) -> ScreenFrame {
        ScreenFrame {
            rows: rows.len() as u16,
            cols: rows.iter().map(|row| row.len()).max().unwrap_or(0) as u16,
            rows_v2: rows
                .iter()
                .map(|row| {
                    vec![ScreenRun {
                        text: row.to_string(),
                        fg: FrameColor::Default,
                        bg: FrameColor::Default,
                        flags: 0,
                        width: row.len() as u16,
                    }]
                })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn cleanup_attach_terminal_leaves_alternate_screen_after_sgr_reset() {
        let text = std::str::from_utf8(RESTORE_AFTER_ATTACH).unwrap();

        let sgr_reset = text.find("\x1b[0m").expect("missing sgr reset");
        let cursor_reset = text.find("\x1b[0 q").expect("missing cursor shape reset");
        let leave_alt = text
            .find("\x1b[?1049l")
            .expect("missing alternate-screen leave");
        assert!(sgr_reset < leave_alt);
        assert!(cursor_reset < leave_alt);
    }

    #[test]
    fn render_screen_frame_applies_cursor_shape() {
        let frame = ScreenFrame {
            rows: 1,
            cols: 1,
            cursor_shape: 5,
            rows_v2: vec![vec![]],
            ..Default::default()
        };
        let mut out = Vec::new();

        FrameRenderer::default().render(&mut out, &frame).unwrap();

        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("\x1b[5 q"));
    }

    #[test]
    fn render_screen_frame_resets_default_cursor_shape() {
        let frame = ScreenFrame {
            rows: 1,
            cols: 1,
            cursor_shape: 0,
            rows_v2: vec![vec![]],
            ..Default::default()
        };
        let mut out = Vec::new();

        FrameRenderer::default().render(&mut out, &frame).unwrap();

        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("\x1b[0 q"));
    }

    #[test]
    fn frame_renderer_skips_unchanged_cursor_shape() {
        let mut renderer = FrameRenderer::default();
        let mut first = frame_with_rows(&["aa"]);
        first.cursor_shape = 5;
        renderer.render(&mut Vec::new(), &first).unwrap();
        let mut second = frame_with_rows(&["bb"]);
        second.cursor_shape = 5;
        let mut out = Vec::new();

        renderer.render(&mut out, &second).unwrap();

        let text = String::from_utf8(out).unwrap();
        assert!(!text.contains("\x1b[5 q"));
    }

    #[test]
    fn frame_renderer_skips_clear_and_unchanged_rows_after_first_frame() {
        let mut renderer = FrameRenderer::default();
        let mut out = Vec::new();
        renderer
            .render(&mut out, &frame_with_rows(&["aa", "bb"]))
            .unwrap();
        out.clear();

        renderer
            .render(&mut out, &frame_with_rows(&["aa", "cc"]))
            .unwrap();

        let text = String::from_utf8(out).unwrap();
        assert!(!text.contains("\x1b[2J"));
        assert!(text.contains("\x1b[2;1H"));
        assert!(!text.contains("aa"));
        assert!(text.contains("cc"));
    }

    #[test]
    fn frame_renderer_skips_identical_frame_body() {
        let mut renderer = FrameRenderer::default();
        let frame = frame_with_rows(&["aa"]);
        let mut out = Vec::new();
        renderer.render(&mut out, &frame).unwrap();
        out.clear();

        renderer.render(&mut out, &frame).unwrap();

        assert!(out.is_empty());
    }

    #[test]
    fn frame_renderer_clears_on_size_change() {
        let mut renderer = FrameRenderer::default();
        let mut out = Vec::new();
        renderer
            .render(&mut out, &frame_with_rows(&["aa"]))
            .unwrap();
        out.clear();

        renderer
            .render(&mut out, &frame_with_rows(&["aaa"]))
            .unwrap();

        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("\x1b[2J"));
    }
}
