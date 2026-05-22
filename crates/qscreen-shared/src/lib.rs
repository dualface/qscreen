use std::path::PathBuf;

pub const PIPE_PREFIX: &str = r"\\.\pipe\qscreen-";

/// Named Pipe 路径：\\.\pipe\qscreen-<sanitized-username>
pub fn pipe_name() -> String {
    let user = current_user();
    format!("{}{}", PIPE_PREFIX, sanitize_pipe_user(&user))
}

/// Daemon 日志路径：%TEMP%\qscreen-daemon-<user>.log
pub fn daemon_log_path() -> PathBuf {
    let temp = std::env::var("TEMP")
        .or_else(|_| std::env::var("TMP"))
        .unwrap_or_else(|_| "C:\\Temp".to_string());
    let user = sanitize_pipe_user(&current_user());
    PathBuf::from(temp).join(format!("qscreen-daemon-{}.log", user))
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
        assert!(pipe_name().starts_with(r"\\.\pipe\qscreen-"));
    }
}
