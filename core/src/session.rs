//! Session termination policy belongs to Rust, not protocol exception handlers.
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
