//! Caller-minted message identifiers compatible with OpenCode's ascending IDs.

/// The number of hexadecimal characters in OpenCode's time/counter prefix.
pub const TIME_PREFIX_WIDTH: usize = 12;
/// The number of base-62 characters following the time/counter prefix.
pub const RANDOM_SUFFIX_WIDTH: usize = 14;
/// The complete suffix width after `msg_`.
pub const OPENCODE_ID_SUFFIX_WIDTH: usize = TIME_PREFIX_WIDTH + RANDOM_SUFFIX_WIDTH;

const BASE62: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
const LOW_48_BITS: u64 = (1_u64 << 48) - 1;

/// Stateful generator for OpenCode-compatible ascending message IDs.
///
/// OpenCode 1.18.10 encodes Unix-millisecond time multiplied by `0x1000`,
/// plus a counter that starts at one for each clock tick, into six bytes. The
/// remaining fourteen characters are base-62 randomness.
#[derive(Clone, Debug, Default)]
pub struct MessageIdGenerator {
    last_timestamp_ms: Option<u64>,
    counter: u64,
}

impl MessageIdGenerator {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            last_timestamp_ms: None,
            counter: 0,
        }
    }

    /// Mints one identifier from a Unix-millisecond clock reading and random
    /// bytes. Each byte is reduced modulo 62 exactly as OpenCode does.
    #[must_use]
    pub fn mint(&mut self, timestamp_ms: u64, random: [u8; RANDOM_SUFFIX_WIDTH]) -> String {
        if self.last_timestamp_ms != Some(timestamp_ms) {
            self.last_timestamp_ms = Some(timestamp_ms);
            self.counter = 0;
        }
        self.counter = self.counter.saturating_add(1);

        let encoded = timestamp_ms.wrapping_mul(0x1000).wrapping_add(self.counter) & LOW_48_BITS;
        let mut id = format!("msg_{encoded:0TIME_PREFIX_WIDTH$x}");
        for byte in random {
            id.push(char::from(BASE62[usize::from(byte) % BASE62.len()]));
        }
        id
    }
}

/// Returns whether an ID has OpenCode's message prefix, lowercase 48-bit
/// clock/counter field, and exact base-62 suffix width.
#[must_use]
pub fn is_opencode_message_id(value: &str) -> bool {
    let Some(suffix) = value.strip_prefix("msg_") else {
        return false;
    };
    if suffix.len() != OPENCODE_ID_SUFFIX_WIDTH || !suffix.is_ascii() {
        return false;
    }
    let (time, random) = suffix.split_at(TIME_PREFIX_WIDTH);
    time.bytes()
        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        && random.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

#[cfg(test)]
mod tests {
    use super::{
        MessageIdGenerator, RANDOM_SUFFIX_WIDTH, TIME_PREFIX_WIDTH, is_opencode_message_id,
    };

    #[test]
    fn contract_is_clock_derived_counter_ordered_and_uses_opencode_widths() {
        let mut generator = MessageIdGenerator::new();
        let first = generator.mint(1_785_000_000_000, [0; RANDOM_SUFFIX_WIDTH]);
        let second = generator.mint(1_785_000_000_000, [61; RANDOM_SUFFIX_WIDTH]);
        let third = generator.mint(1_785_000_000_001, [0; RANDOM_SUFFIX_WIDTH]);

        assert!(is_opencode_message_id(&first));
        assert!(is_opencode_message_id(&second));
        assert!(is_opencode_message_id(&third));
        assert_eq!(first.len(), "msg_".len() + TIME_PREFIX_WIDTH + 14);
        assert!(first < second, "same-tick counter must sort ascending");
        assert!(second < third, "the next clock tick must sort ascending");
        assert_eq!(&first[4..16], "f9a4a7a00001");
    }

    #[test]
    fn readable_and_wrong_width_schemes_are_rejected() {
        for invalid in [
            "msg_oca_w4f2a1_1",
            "msg_zzzzzzzzzzzzzzzzzzzzzzzzzz",
            "msg_000000000001short",
            "msg_00000000000Gaaaaaaaaaaaaaa",
            "evt_000000000001aaaaaaaaaaaaaa",
        ] {
            assert!(!is_opencode_message_id(invalid), "accepted {invalid}");
        }
    }
}
