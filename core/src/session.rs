//! Session termination policy belongs to Rust, not protocol exception handlers.
/// Do not reset a crash-loop budget merely because a process announced login.
pub fn recovery_is_stable(
    expected_pid: Option<u32>,
    current_pid: Option<u32>,
    running: bool,
    ready: bool,
) -> bool {
    expected_pid.is_some() && expected_pid == current_pid && running && ready
}

pub fn authentication_failure(message: &str) -> bool {
    message
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .any(|code| {
            matches!(
                code,
                "NOT_AUTHORIZED"
                    | "NOT_AUTHORIZED_DEVICE"
                    | "LOGGED_OUT"
                    | "V3_TOKEN_CLIENT_LOGGED_OUT"
                    | "AUTHENTICATION"
                    | "AUTHENTICATION_FAILED"
                    | "AUTHENTICATION_ERROR"
            )
        })
}
