use std::path::PathBuf;

pub const PIPE_PREFIX: &str = r"\\.\pipe\qscreen-";

/// IPC 名称：Windows 为 Named Pipe，Unix 为 Unix domain socket 路径。
pub fn pipe_name() -> String {
    let user = current_user();
    #[cfg(windows)]
    {
        format!("{}{}", PIPE_PREFIX, sanitize_pipe_user(&user))
    }
    #[cfg(unix)]
    {
        unix_runtime_dir()
            .join(format!("qscreen-{}.sock", sanitize_pipe_user(&user)))
            .to_string_lossy()
            .into_owned()
    }
}

/// Daemon 日志路径：Windows 用 %TEMP%，Unix 用 ${TMPDIR:-/tmp}。
pub fn daemon_log_path() -> PathBuf {
    let user = sanitize_pipe_user(&current_user());
    #[cfg(windows)]
    {
        let temp = std::env::var("TEMP")
            .or_else(|_| std::env::var("TMP"))
            .unwrap_or_else(|_| "C:\\Temp".to_string());
        PathBuf::from(temp).join(format!("qscreen-daemon-{}.log", user))
    }
    #[cfg(unix)]
    {
        unix_runtime_dir().join(format!("qscreen-daemon-{}.log", user))
    }
}

/// Daemon single-instance lock path.
#[cfg(unix)]
pub fn daemon_lock_path() -> PathBuf {
    unix_runtime_dir().join(format!(
        "qscreen-{}.lock",
        sanitize_pipe_user(&current_user())
    ))
}

pub fn current_user() -> String {
    std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_else(|_| "unknown".to_string())
}

/// 仅保留 [A-Za-z0-9_-]，其余字符替换为 '_'
pub fn sanitize_pipe_user(user: &str) -> String {
    user.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(unix)]
fn unix_runtime_dir() -> PathBuf {
    std::env::var_os("TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_keeps_safe_chars() {
        assert_eq!(sanitize_pipe_user("user-name_01"), "user-name_01");
    }

    #[test]
    fn sanitize_replaces_unsafe() {
        assert_eq!(sanitize_pipe_user("user@domain"), "user_domain");
        assert_eq!(sanitize_pipe_user("DOMAIN\\user"), "DOMAIN_user");
    }

    #[test]
    fn pipe_name_has_prefix() {
        #[cfg(windows)]
        assert!(pipe_name().starts_with(r"\\.\pipe\qscreen-"));
        #[cfg(unix)]
        assert!(pipe_name().ends_with(".sock"));
    }
}
