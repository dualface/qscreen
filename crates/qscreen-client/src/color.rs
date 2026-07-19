//! 运行环境色彩支持检测与着色辅助。
//!
//! qscn 启动时(见 [`init_and_record`])检测当前终端是否支持 ANSI 色彩,把结果
//! 缓存进原子变量,并在需要时记录到客户端日志。检测遵循常见约定:
//!
//! - `QSCREEN_COLOR=always|never|auto` —— 显式覆盖(优先级最高);
//! - `NO_COLOR` —— 只要存在即禁用(遵循 <https://no-color.org/>);
//! - `CLICOLOR_FORCE` —— 非空且非 `0` 时强制启用(即便不是 tty);
//! - `TERM=dumb` —— 禁用;
//! - stdout / stderr 是否为交互式终端;
//! - Windows 上还会尝试开启控制台 VT 输出处理。
//!
//! 着色只使用标准 ANSI 颜色(不硬编码 RGB、不使用纯黑/纯白前景),让终端按自身
//! light/dark 主题映射到合适的深浅,从而在两种主题下都保持可读。
//!
//! 缓存默认处于「未初始化」状态,此时 [`supported`] 返回 `false`。只有生产路径的
//! [`init_and_record`] 会真正探测环境并落定结果,因此单元测试里的渲染逻辑始终走
//! 无色分支,输出保持确定。

use std::io::IsTerminal;
use std::sync::atomic::{AtomicU8, Ordering};

const UNINIT: u8 = 0;
const ON: u8 = 1;
const OFF: u8 = 2;

static STDOUT_STATE: AtomicU8 = AtomicU8::new(UNINIT);
static STDERR_STATE: AtomicU8 = AtomicU8::new(UNINIT);

/// 参与决策的运行环境快照,便于用纯函数 [`decide`] 做单元测试。
#[derive(Debug, Clone)]
struct DetectEnv {
    qscreen_color: Option<String>,
    no_color: bool,
    clicolor_force: bool,
    term_dumb: bool,
    is_tty: bool,
}

/// 主题安全的 SGR 参数表。颜色统一取标准(非 bright)色号,由终端按主题解释。
pub mod sgr {
    /// 会话 id:加粗青色。
    pub const ID: &str = "1;36";
    /// 会话名:加粗(跟随默认前景色,深浅由主题决定)。
    pub const NAME: &str = "1";
    /// 创建时间:暗淡。
    pub const CREATED: &str = "2";
    /// 终端尺寸:青色。
    pub const SIZE: &str = "36";
    /// 工作目录:蓝色(在 light/dark 下都有足够对比度)。
    pub const CWD: &str = "34";
    /// 帮助标题 / 分节标题:加粗青色。
    pub const HEADER: &str = "1;36";
    /// 次要提示行:暗淡。
    pub const HINT: &str = "2";
    /// 错误信息:加粗红色。
    pub const ERROR: &str = "1;31";
    /// 当前会话标记 `*`:绿色。
    pub const CURRENT: &str = "32";
    /// 成功类状态:绿色。
    pub const SUCCESS: &str = "32";
}

/// 根据会话状态选择颜色:退出=红、attached=绿、detached=蓝。
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

/// 纯函数:根据环境快照决定是否启用色彩,并返回可读的判定原因(便于记录)。
fn decide(env: &DetectEnv) -> (bool, &'static str) {
    if let Some(value) = env.qscreen_color.as_deref() {
        match value.trim().to_ascii_lowercase().as_str() {
            "always" | "force" | "1" | "yes" | "on" => return (true, "QSCREEN_COLOR=always"),
            "never" | "0" | "no" | "off" => return (false, "QSCREEN_COLOR=never"),
            // auto 或无法识别的值:回落到自动探测
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

/// 在给定 tty 判定下计算最终启用状态。仅当输出是真正的交互式控制台时才需要开启
/// VT 处理:若此时 Windows 无法开启 VT,裸转义序列会显示成乱码,故降级为不启用。
/// 被强制着色写入管道/文件时(非 tty)无需 VT,直接照常输出转义序列。
fn resolve(is_tty: bool) -> (bool, &'static str) {
    let env = read_env(is_tty);
    let (mut enabled, mut reason) = decide(&env);
    if enabled && is_tty && !crate::term::enable_windows_vt_output() {
        enabled = false;
        reason = "windows console lacks VT support";
    }
    (enabled, reason)
}

/// qscn 启动时调用:探测 stdout/stderr 色彩支持,落定缓存并记录结果。
pub fn init_and_record() {
    let (stdout_enabled, stdout_reason) = resolve(std::io::stdout().is_terminal());
    STDOUT_STATE.store(if stdout_enabled { ON } else { OFF }, Ordering::Relaxed);

    let (stderr_enabled, _) = resolve(std::io::stderr().is_terminal());
    STDERR_STATE.store(if stderr_enabled { ON } else { OFF }, Ordering::Relaxed);

    record(stdout_enabled, stdout_reason);
}

/// stdout 是否应当着色。未 [`init_and_record`] 前恒为 `false`。
pub fn supported() -> bool {
    STDOUT_STATE.load(Ordering::Relaxed) == ON
}

/// stderr 是否应当着色(错误信息走 stderr)。
pub fn stderr_supported() -> bool {
    STDERR_STATE.load(Ordering::Relaxed) == ON
}

/// 用 SGR 参数包裹一段文本;stdout 不支持色彩时原样返回。
pub fn paint(text: &str, sgr: &str) -> String {
    if supported() {
        wrap(text, sgr)
    } else {
        text.to_string()
    }
}

/// 与 [`paint`] 类似,但针对 stderr(错误信息)。
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

/// 把检测结果写入客户端日志(仅在设置了 `QSCREEN_DEBUG` 时),方便排查
/// 「为什么没有颜色 / 为什么有颜色」。这是「记录检查结果」的持久化部分,
/// 进程内的缓存(见上）则是即时可用的那份记录。
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
        // 显式 always 覆盖 NO_COLOR / 非 tty
        let (enabled, reason) = decide(&env(Some("always"), true, false, false, false));
        assert!(enabled);
        assert_eq!(reason, "QSCREEN_COLOR=always");

        // 显式 never 覆盖 tty / CLICOLOR_FORCE
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
        // 未初始化时缓存为 UNINIT,不应着色。
        assert_eq!(paint("hi", sgr::ID), "hi");
    }

    #[test]
    fn wrap_produces_sgr_sequence() {
        assert_eq!(wrap("x", "1;36"), "\x1b[1;36mx\x1b[0m");
    }
}
