use std::io::{IsTerminal, Write};

use qscreen_protocol::{
    FRAME_FLAG_BLINK, FRAME_FLAG_BOLD, FRAME_FLAG_DIM, FRAME_FLAG_HIDDEN, FRAME_FLAG_INVERSE,
    FRAME_FLAG_ITALIC, FRAME_FLAG_STRIKETHROUGH, FRAME_FLAG_UNDERLINE, FrameColor,
    FrameMouseEncoding, FrameMouseMode, ScreenFrame,
};
use unicode_width::UnicodeWidthStr;

const PREPARE_ATTACH: &[u8] = b"\x1b[?1049h\x1b[?1004h\x1b[2J\x1b[H";
const RESTORE_AFTER_ATTACH: &[u8] =
    b"\x1b[?2026l\x1b[?1l\x1b[?2004l\x1b[?9l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1005l\x1b[?1006l\x1b[?1004l\x1b[?25h\x1b[0m\x1b[0 q\x1b[r\x1b[?1049l";

/// Environment preflight before attach:
/// 1. Require both stdin/stdout to be interactive terminals (otherwise rendering escape sequences is meaningless);
/// 2. On Windows, proactively enable console VT output processing so even legacy conhost renders correctly;
///    on failure, only warn without aborting (leaving it to the user to switch to Windows Terminal).
pub fn preflight_interactive() -> anyhow::Result<()> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        anyhow::bail!(
            "qscn attach 需要交互式终端,但 stdin/stdout 被重定向了;\
             请直接在终端窗口里运行,不要通过管道或重定向"
        );
    }

    #[cfg(windows)]
    if let Err(e) = enable_virtual_terminal_output() {
        eprintln!(
            "warning: 无法开启终端 VT 输出处理({e});画面可能显示为乱码,建议使用 Windows Terminal"
        );
    }

    Ok(())
}

/// Try to enable VT output processing on the Windows console, returning whether
/// it is available. Non-attach commands (such as `ls` and colored help) also
/// depend on it, so color detection calls this once before coloring.
/// On non-Windows platforms ANSI is supported by default, so this is always `true`.
#[cfg(windows)]
pub fn enable_windows_vt_output() -> bool {
    enable_virtual_terminal_output().is_ok()
}

#[cfg(not(windows))]
pub fn enable_windows_vt_output() -> bool {
    true
}

/// Enable `ENABLE_VIRTUAL_TERMINAL_PROCESSING` on the stdout console so raw
/// ANSI/VT escape sequences are interpreted correctly by the Windows console
/// (including legacy conhost).
#[cfg(windows)]
fn enable_virtual_terminal_output() -> std::io::Result<()> {
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Console::{
        ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode, GetStdHandle, STD_OUTPUT_HANDLE,
        SetConsoleMode,
    };

    // SAFETY: we only fetch the standard output handle and read/modify the console mode; the handle does not need to be released.
    unsafe {
        let handle = GetStdHandle(STD_OUTPUT_HANDLE);
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error());
        }

        let mut mode: u32 = 0;
        if GetConsoleMode(handle, &mut mode) == 0 {
            return Err(std::io::Error::last_os_error());
        }

        if mode & ENABLE_VIRTUAL_TERMINAL_PROCESSING == 0
            && SetConsoleMode(handle, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING) == 0
        {
            return Err(std::io::Error::last_os_error());
        }
    }

    Ok(())
}

/// Enable or disable mouse reporting on the Windows console input handle,
/// following the state of the attached application's mouse tracking.
///
/// Like GNU screen, qscn relays the inner application's mouse events: it
/// mirrors the DEC private mouse modes to the host terminal and re-encodes the
/// events back to the child. On Windows that relay only works if the client's
/// own console input delivers mouse records — which requires
/// `ENABLE_MOUSE_INPUT` (and `ENABLE_QUICK_EDIT_MODE` cleared, so the console
/// does not swallow the mouse for its own selection). `ENABLE_EXTENDED_FLAGS`
/// is required for the quick-edit change to take effect. When the application
/// has no mouse mode we restore quick-edit so host-side selection keeps working.
///
/// No-op on non-Windows, where the host terminal drives mouse reporting purely
/// through the VT private-mode sequences and delivers events on stdin.
#[cfg(windows)]
pub fn set_console_mouse_capture(enabled: bool) -> std::io::Result<()> {
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Console::{
        ENABLE_EXTENDED_FLAGS, ENABLE_MOUSE_INPUT, ENABLE_QUICK_EDIT_MODE, GetConsoleMode,
        GetStdHandle, STD_INPUT_HANDLE, SetConsoleMode,
    };

    // SAFETY: we only fetch the standard input handle and read/modify the console mode; the handle does not need to be released.
    unsafe {
        let handle = GetStdHandle(STD_INPUT_HANDLE);
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error());
        }

        let mut mode: u32 = 0;
        if GetConsoleMode(handle, &mut mode) == 0 {
            return Err(std::io::Error::last_os_error());
        }

        let new_mode = if enabled {
            (mode | ENABLE_EXTENDED_FLAGS | ENABLE_MOUSE_INPUT) & !ENABLE_QUICK_EDIT_MODE
        } else {
            (mode | ENABLE_EXTENDED_FLAGS | ENABLE_QUICK_EDIT_MODE) & !ENABLE_MOUSE_INPUT
        };

        if new_mode != mode && SetConsoleMode(handle, new_mode) == 0 {
            return Err(std::io::Error::last_os_error());
        }
    }

    Ok(())
}

#[cfg(not(windows))]
pub fn set_console_mouse_capture(_enabled: bool) -> std::io::Result<()> {
    Ok(())
}

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
    input_modes: InputModeState,
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

        render_input_mode_diff(out, &mut self.input_modes, frame)?;
        render_screen_frame_with_previous(out, frame, self.previous.as_ref(), force_full)?;
        self.previous = Some(frame.clone());
        Ok(())
    }

    pub fn reset(&mut self) {
        self.previous = None;
    }

    pub fn input_modes(&self) -> InputModeState {
        self.input_modes
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct InputModeState {
    pub application_cursor: bool,
    pub bracketed_paste: bool,
    pub mouse_mode: FrameMouseMode,
    pub mouse_encoding: FrameMouseEncoding,
}

fn render_input_mode_diff<W: Write>(
    out: &mut W,
    current: &mut InputModeState,
    frame: &ScreenFrame,
) -> std::io::Result<()> {
    if current.application_cursor != frame.application_cursor {
        out.write_all(if frame.application_cursor {
            b"\x1b[?1h"
        } else {
            b"\x1b[?1l"
        })?;
        current.application_cursor = frame.application_cursor;
    }
    if current.bracketed_paste != frame.bracketed_paste {
        out.write_all(if frame.bracketed_paste {
            b"\x1b[?2004h"
        } else {
            b"\x1b[?2004l"
        })?;
        current.bracketed_paste = frame.bracketed_paste;
    }
    if current.mouse_mode != frame.mouse_mode {
        write_mouse_mode(out, current.mouse_mode, false)?;
        write_mouse_mode(out, frame.mouse_mode, true)?;
        // Windows: also toggle console mouse input so the attach loop receives
        // mouse records to relay; best-effort no-op elsewhere.
        let _ = set_console_mouse_capture(frame.mouse_mode != FrameMouseMode::None);
        current.mouse_mode = frame.mouse_mode;
    }
    if current.mouse_encoding != frame.mouse_encoding {
        write_mouse_encoding(out, current.mouse_encoding, false)?;
        write_mouse_encoding(out, frame.mouse_encoding, true)?;
        current.mouse_encoding = frame.mouse_encoding;
    }
    Ok(())
}

fn write_mouse_mode<W: Write>(
    out: &mut W,
    mode: FrameMouseMode,
    enabled: bool,
) -> std::io::Result<()> {
    let code = match mode {
        FrameMouseMode::None => return Ok(()),
        FrameMouseMode::Press => 9,
        FrameMouseMode::PressRelease => 1000,
        FrameMouseMode::ButtonMotion => 1002,
        FrameMouseMode::AnyMotion => 1003,
    };
    write!(out, "\x1b[?{}{}", code, if enabled { "h" } else { "l" })
}

fn write_mouse_encoding<W: Write>(
    out: &mut W,
    encoding: FrameMouseEncoding,
    enabled: bool,
) -> std::io::Result<()> {
    let code = match encoding {
        FrameMouseEncoding::Default => return Ok(()),
        FrameMouseEncoding::Utf8 => 1005,
        FrameMouseEncoding::Sgr => 1006,
    };
    write!(out, "\x1b[?{}{}", code, if enabled { "h" } else { "l" })
}

fn render_screen_frame_with_previous<W: Write>(
    out: &mut W,
    frame: &ScreenFrame,
    previous: Option<&ScreenFrame>,
    force_full: bool,
) -> std::io::Result<()> {
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
    fn prepare_attach_terminal_enters_host_alternate_screen() {
        let text = std::str::from_utf8(PREPARE_ATTACH).unwrap();

        assert!(text.contains("\x1b[?1049h"));
        assert!(text.contains("\x1b[2J"));
    }

    #[test]
    fn cleanup_attach_terminal_leaves_host_alternate_screen() {
        let text = std::str::from_utf8(RESTORE_AFTER_ATTACH).unwrap();

        assert!(text.contains("\x1b[0m"));
        assert!(text.contains("\x1b[0 q"));
        assert!(text.contains("\x1b[?1049l"));
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
    fn frame_renderer_does_not_reenter_host_alternate_screen() {
        let mut frame = frame_with_rows(&["aa"]);
        frame.alternate_screen = true;
        let mut out = Vec::new();

        FrameRenderer::default().render(&mut out, &frame).unwrap();

        let text = String::from_utf8(out).unwrap();
        assert!(!text.contains("\x1b[?1049h"));
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

    #[test]
    fn frame_renderer_applies_input_modes() {
        let mut renderer = FrameRenderer::default();
        let mut frame = frame_with_rows(&["aa"]);
        frame.application_cursor = true;
        frame.bracketed_paste = true;
        frame.mouse_mode = FrameMouseMode::PressRelease;
        frame.mouse_encoding = FrameMouseEncoding::Sgr;
        let mut out = Vec::new();

        renderer.render(&mut out, &frame).unwrap();

        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("\x1b[?1h"));
        assert!(text.contains("\x1b[?2004h"));
        assert!(text.contains("\x1b[?1000h"));
        assert!(text.contains("\x1b[?1006h"));
    }
}
