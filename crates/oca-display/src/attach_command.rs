//! Shared argv construction for headed OpenCode attachments.

/// Builds the complete OpenCode argv used by every headed display backend.
#[must_use]
pub fn opencode_attach_argv(base_url: &str, session_id: &str) -> Vec<String> {
    vec![
        "opencode".to_owned(),
        "attach".to_owned(),
        base_url.to_owned(),
        "--session".to_owned(),
        session_id.to_owned(),
    ]
}

#[cfg(test)]
mod tests {
    use super::opencode_attach_argv;

    #[test]
    fn pins_the_shared_attach_argv_shape() {
        assert_eq!(
            opencode_attach_argv("http://127.0.0.1:4096/", "ses_target"),
            [
                "opencode",
                "attach",
                "http://127.0.0.1:4096/",
                "--session",
                "ses_target",
            ]
        );
    }
}
