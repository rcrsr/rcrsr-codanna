//! Time-ordered opaque identifier for index generations.
//!
//! A [`GenerationId`] is a 19-character lowercase-hex string: 13 hex digits
//! of a Unix-millis timestamp followed by 6 hex digits of randomness. It is
//! deliberately compared as a plain string (`Ord`/`PartialOrd` derive from
//! the field order) rather than parsed back into a timestamp, so ordering
//! stays cheap and infallible while still sorting new generations after old
//! ones by construction.

use crate::error::{IndexError, IndexResult};
use rand::RngExt;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

/// Length, in hex characters, of the timestamp portion.
const TIMESTAMP_HEX_LEN: usize = 13;
/// Length, in hex characters, of the random suffix.
const RANDOM_HEX_LEN: usize = 6;
/// Total length, in hex characters, of a valid [`GenerationId`].
const TOTAL_HEX_LEN: usize = TIMESTAMP_HEX_LEN + RANDOM_HEX_LEN;

/// A time-ordered, opaque identifier for an index generation.
///
/// Construct via [`GenerationId::generate`] for a fresh id, or
/// [`GenerationId::new`] to validate an existing string (e.g. one read back
/// from disk or supplied on the CLI).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct GenerationId(String);

impl GenerationId {
    /// Validate `s` as a generation id: exactly 19 lowercase
    /// hex characters. Returns `None` on any other input rather than
    /// panicking.
    pub fn new(s: &str) -> Option<Self> {
        if s.len() != TOTAL_HEX_LEN {
            return None;
        }
        if !s
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return None;
        }
        Some(Self(s.to_string()))
    }

    /// Generate a fresh id from the current time and 3 random bytes.
    ///
    /// The timestamp portion is the Unix-millis time zero-padded to
    /// 13 hex digits, so ids generated later sort after
    /// ids generated earlier under plain string `Ord`.
    pub fn generate() -> Self {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);

        let mut rng = rand::rng();
        let random_bytes: [u8; 3] = rng.random();
        let random = u32::from_be_bytes([0, random_bytes[0], random_bytes[1], random_bytes[2]]);

        let id = format!("{millis:013x}{random:06x}");
        debug_assert_eq!(id.len(), TOTAL_HEX_LEN);

        Self(id)
    }

    /// Borrow the underlying hex string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for GenerationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for GenerationId {
    type Err = IndexError;

    fn from_str(s: &str) -> IndexResult<Self> {
        Self::new(s).ok_or_else(|| IndexError::InvalidGenerationId {
            input: s.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;
    use std::time::Duration;

    #[test]
    fn generate_ids_sort_ascending_by_construction_order() {
        let first = GenerationId::generate();
        sleep(Duration::from_millis(5));
        let second = GenerationId::generate();

        assert!(first < second);
    }

    #[test]
    fn new_accepts_a_generate_output() {
        let generated = GenerationId::generate();

        let reparsed = GenerationId::new(generated.as_str());

        assert_eq!(reparsed, Some(generated));
    }

    #[test]
    fn new_rejects_wrong_length() {
        assert_eq!(GenerationId::new("abc123"), None);
        assert_eq!(GenerationId::new("0123456789abcdef0123456789abcdef0"), None);
    }

    #[test]
    fn new_rejects_uppercase() {
        let input = "0123456789ABCDEF012";
        assert_eq!(input.len(), 19);
        assert_eq!(GenerationId::new(input), None);
    }

    #[test]
    fn new_rejects_non_hex_chars() {
        assert_eq!(GenerationId::new("0123456789zzzzz0123"), None);
    }

    #[test]
    fn new_rejects_empty_string() {
        assert_eq!(GenerationId::new(""), None);
    }

    #[test]
    fn display_and_from_str_round_trip() {
        let id = GenerationId::generate();

        let rendered = id.to_string();
        let parsed: GenerationId = rendered.parse().expect("valid generation id string");

        assert_eq!(parsed, id);
    }

    #[test]
    fn from_str_rejects_invalid_input() {
        let result: IndexResult<GenerationId> = "not-a-valid-id".parse();

        assert!(matches!(
            result,
            Err(IndexError::InvalidGenerationId { .. })
        ));
    }
}
