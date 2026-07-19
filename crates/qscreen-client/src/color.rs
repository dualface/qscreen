//! Runtime color-support detection and coloring helpers.
//!
//! At startup qscn (see [`init_and_record`]) detects whether the current
//! terminal supports ANSI color, caches the result in atomic variables, and
//! optionally records it to the client log. Detection follows common
//! conventions:
//!
//! - `QSCREEN_COLOR=always|never|auto` — explicit override (highest priority);
//! - `NO_COLOR` — disables whenever present (per <https://no-color.org/>);
//! - `CLICOLOR_FORCE` — forces color on when non-empty and not `0` (even if not a tty);
//! - `TERM=dumb` — disables;
//! - whether stdout / stderr is an interactive terminal;
//! - on Windows, also tries to enable console VT output processing.
//!
//! Coloring uses only standard ANSI colors (no hardcoded RGB, no pure
//! black/white foreground), letting the terminal map them to suitable shades
//! per its own light/dark theme so output stays readable under both.
//!
//! The cache defaults to an "uninitialized" state, in which [`supported`]
//! returns `false`. Only the production path [`init_and_record`] actually
//! probes the environment and settles the result, so rendering logic in unit
//! tests always takes the colorless branch and output stays deterministic.

use std::io::IsTerminal;
use std::sync::atomic::{AtomicU8, Ordering};

const UNINIT: u8 = 0;
const ON: u8 = 1;
const OFF: u8 = 2;

static STDOUT_STATE: AtomicU8 = AtomicU8::new(UNINIT);
static STDERR_STATE: AtomicU8 = AtomicU8::new(UNINIT);

/// Snapshot of the runtime environment used for the decision, so the pure
/// function [`decide`] can be unit tested.
#[derive(Debug, Clone)]
struct DetectEnv {
    qscreen_color: Option<String>,
    no_color: bool,
    clicolor_force: bool,
    term_dumb: bool,
    is_tty: bool,
}

/// Theme-safe SGR parameter table. Colors use standard (non-bright) codes,
/// interpreted by the terminal according to its theme.
pub mod sgr {
    /// Session id: bold cyan.
    pub const ID: &str = "1;36";
    /// Session name: bold (follows the default foreground; shade set by theme).
    pub const NAME: &str = "1";
    /// Creation time: dim.
    pub const CREATED: &str = "2";
    /// Terminal size: cyan.
    pub const SIZE: &str = "36";
    /// Working directory: blue (enough contrast under both light/dark).
    pub const CWD: &str = "34";
    /// Help title / section heading: bold cyan.
    pub const HEADER: &str = "1;36";
    /// Secondary hint line: dim.
    pub const HINT: &str = "2";
    /// Shortcut keys in the hint line: bold yellow, standing out from the dim description text.
    pub const KEY: &str = "1;33";
    /// Error message: bold red.
    pub const ERROR: &str = "1;31";
    /// Current-session marker `*`: green.
    pub const CURRENT: &str = "32";
    /// Success status: green.
    pub const SUCCESS: &str = "32";
}

/// Pick a color by session state: exited=red, attached=green, detached=blue.
pub fn state_sgr(exited: bool, attached: bool) -> &'static str {
    if exited {
        "1;31"
    } else if attached {
        "32"
    } else {
        "34"
    }
}

fn read_env(is_tty: bool) -> DetectEnv {
    let term = std::env::var("TERM").unwrap_or_default();
    DetectEnv {
        qscreen_color: std::env::var("QSCREEN_COLOR").ok(),
        no_color: std::env::var_os("NO_COLOR").is_some(),
        clicolor_force: std::env::var_os("CLICOLOR_FORCE")
            .is_some_and(|v| !v.is_empty() && v != "0"),
        term_dumb: term.eq_ignore_ascii_case("dumb"),
        is_tty,
    }
}

/// Pure function: decide whether to enable color from the environment
/// snapshot, returning a human-readable reason (for logging).
fn decide(env: &DetectEnv) -> (bool, &'static str) {
    if let Some(value) = env.qscreen_color.as_deref() {
        match value.trim().to_ascii_lowercase().as_str() {
            "always" | "force" | "1" | "yes" | "on" => return (true, "QSCREEN_COLOR=always"),
            "never" | "0" | "no" | "off" => return (false, "QSCREEN_COLOR=never"),
            // auto or an unrecognized value: fall back to auto-detection
            _ => {}
        }
    }
    if env.no_color {
        return (false, "NO_COLOR is set");
    }
    if env.clicolor_force {
        return (true, "CLICOLOR_FORCE is set");
    }
    if env.term_dumb {
        return (false, "TERM=dumb");
    }
    if !env.is_tty {
        return (false, "output is not a terminal");
    }
    (true, "interactive terminal")
}

/// Compute the final enabled state given the tty verdict. VT processing is
/// only needed when output is a genuine interactive console: if Windows cannot
/// enable VT then, raw escape sequences would show as garbage, so we downgrade
/// to disabled. When color is forced onto a pipe/file (non-tty), VT is not
/// needed and escape sequences are emitted as usual.
fn resolve(is_tty: bool) -> (bool, &'static str) {
    let env = read_env(is_tty);
    let (mut enabled, mut reason) = decide(&env);
    if enabled && is_tty && !crate::term::enable_windows_vt_output() {
        enabled = false;
        reason = "windows console lacks VT support";
    }
    (enabled, reason)
}

/// Called at qscn startup: probe stdout/stderr color support, settle the cache,
/// and record the result.
pub fn init_and_record() {
    let (stdout_enabled, stdout_reason) = resolve(std::io::stdout().is_terminal());
    STDOUT_STATE.store(if stdout_enabled { ON } else { OFF }, Ordering::Relaxed);

    let (stderr_enabled, _) = resolve(std::io::stderr().is_terminal());
    STDERR_STATE.store(if stderr_enabled { ON } else { OFF }, Ordering::Relaxed);

    record(stdout_enabled, stdout_reason);
}

/// Whether stdout should be colored. Always `false` before [`init_and_record`].
pub fn supported() -> bool {
    STDOUT_STATE.load(Ordering::Relaxed) == ON
}

/// Whether stderr should be colored (error messages go to stderr).
pub fn stderr_supported() -> bool {
    STDERR_STATE.load(Ordering::Relaxed) == ON
}

/// Wrap text in SGR parameters; returns it unchanged when stdout lacks color support.
pub fn paint(text: &str, sgr: &str) -> String {
    if supported() {
        wrap(text, sgr)
    } else {
        text.to_string()
    }
}

/// Like [`paint`], but for stderr (error messages).
pub fn paint_err(text: &str, sgr: &str) -> String {
    if stderr_supported() {
        wrap(text, sgr)
    } else {
        text.to_string()
    }
}

fn wrap(text: &str, sgr: &str) -> String {
    format!("\x1b[{sgr}m{text}\x1b[0m")
}

/// Write the detection result to the client log (only when `QSCREEN_DEBUG` is
/// set), to help diagnose "why is there no color / why is there color". This is
/// the persistent part of recording the check; the in-process cache (see above)
/// is the immediately usable copy.
fn record(enabled: bool, reason: &str) {
    if !debug_logging_enabled() {
        return;
    }
    let line = format!(
        "{} color-support enabled={} reason=\"{}\"\n",
        chrono::Local::now().format("%Y-%m-%dT%H:%M:%S"),
        enabled,
        reason
    );
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(qscreen_shared::client_log_path())
    {
        use std::io::Write;
        let _ = file.write_all(line.as_bytes());
    }
}

fn debug_logging_enabled() -> bool {
    std::env::var_os("QSCREEN_DEBUG").is_some_and(|v| !v.is_empty() && v != "0")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(
        qscreen_color: Option<&str>,
        no_color: bool,
        clicolor_force: bool,
        term_dumb: bool,
        is_tty: bool,
    ) -> DetectEnv {
        DetectEnv {
            qscreen_color: qscreen_color.map(|s| s.to_string()),
            no_color,
            clicolor_force,
            term_dumb,
            is_tty,
        }
    }

    #[test]
    fn tty_without_overrides_enables_color() {
        let (enabled, _) = decide(&env(None, false, false, false, true));
        assert!(enabled);
    }

    #[test]
    fn non_tty_disables_color() {
        let (enabled, reason) = decide(&env(None, false, false, false, false));
        assert!(!enabled);
        assert_eq!(reason, "output is not a terminal");
    }

    #[test]
    fn no_color_disables_even_on_tty() {
        let (enabled, reason) = decide(&env(None, true, false, false, true));
        assert!(!enabled);
        assert_eq!(reason, "NO_COLOR is set");
    }

    #[test]
    fn clicolor_force_enables_without_tty() {
        let (enabled, reason) = decide(&env(None, false, true, false, false));
        assert!(enabled);
        assert_eq!(reason, "CLICOLOR_FORCE is set");
    }

    #[test]
    fn no_color_wins_over_clicolor_force() {
        let (enabled, _) = decide(&env(None, true, true, false, true));
        assert!(!enabled);
    }

    #[test]
    fn term_dumb_disables_color() {
        let (enabled, reason) = decide(&env(None, false, false, true, true));
        assert!(!enabled);
        assert_eq!(reason, "TERM=dumb");
    }

    #[test]
    fn qscreen_color_override_takes_precedence() {
        // explicit always overrides NO_COLOR / non-tty
        let (enabled, reason) = decide(&env(Some("always"), true, false, false, false));
        assert!(enabled);
        assert_eq!(reason, "QSCREEN_COLOR=always");

        // explicit never overrides tty / CLICOLOR_FORCE
        let (enabled, reason) = decide(&env(Some("never"), false, true, false, true));
        assert!(!enabled);
        assert_eq!(reason, "QSCREEN_COLOR=never");
    }

    #[test]
    fn qscreen_color_auto_falls_back_to_detection() {
        let (enabled, _) = decide(&env(Some("auto"), false, false, false, true));
        assert!(enabled);
        let (enabled, _) = decide(&env(Some("auto"), false, false, false, false));
        assert!(!enabled);
    }

    #[test]
    fn state_sgr_maps_states_to_distinct_colors() {
        assert_eq!(state_sgr(true, false), "1;31");
        assert_eq!(state_sgr(false, true), "32");
        assert_eq!(state_sgr(false, false), "34");
    }

    #[test]
    fn paint_is_plain_before_init() {
        // before init the cache is UNINIT, so nothing should be colored.
        assert_eq!(paint("hi", sgr::ID), "hi");
    }

    #[test]
    fn wrap_produces_sgr_sequence() {
        assert_eq!(wrap("x", "1;36"), "\x1b[1;36mx\x1b[0m");
    }
}
