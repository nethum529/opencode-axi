//! Display-mode selection for a newly dispatched worker.

/// The display backend selected for one dispatch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DisplayMode {
    /// Attach the OpenCode TUI through herdr.
    Herdr,
    /// Attach the OpenCode TUI in a detached tmux window.
    Tmux,
    /// Run without a display attachment.
    Headless,
}

impl DisplayMode {
    /// Selects the first available display mode in the binding fallback order.
    ///
    /// `herdr_available` is deliberately lazy. An explicit headless dispatch
    /// returns before invoking it, so opting out never performs discovery.
    pub fn select(
        headless: bool,
        herdr_available: impl FnOnce() -> bool,
        inside_tmux: bool,
    ) -> Self {
        if headless {
            return Self::Headless;
        }
        if herdr_available() {
            return Self::Herdr;
        }
        if inside_tmux {
            return Self::Tmux;
        }
        Self::Headless
    }

    /// Returns the stable value persisted in a ref record.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Herdr => "herdr",
            Self::Tmux => "tmux",
            Self::Headless => "headless",
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::DisplayMode;

    #[test]
    fn explicit_headless_skips_herdr_discovery() {
        let discoveries = AtomicUsize::new(0);

        let selected = DisplayMode::select(
            true,
            || {
                discoveries.fetch_add(1, Ordering::SeqCst);
                true
            },
            true,
        );

        assert_eq!(selected, DisplayMode::Headless);
        assert_eq!(discoveries.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn fallback_order_prefers_herdr_then_tmux_then_headless() {
        assert_eq!(
            DisplayMode::select(false, || true, true),
            DisplayMode::Herdr
        );
        assert_eq!(
            DisplayMode::select(false, || false, true),
            DisplayMode::Tmux
        );
        assert_eq!(
            DisplayMode::select(false, || false, false),
            DisplayMode::Headless
        );
    }
}
