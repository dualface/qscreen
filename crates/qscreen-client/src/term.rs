use std::io::Write;

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
