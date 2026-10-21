//! Validated text primitives used across the domain.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Why a piece of free text could not be accepted into the domain.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TextError {
    /// The input held nothing but whitespace.
    #[error("text is empty")]
    Empty,
    /// The input exceeded the maximum accepted length.
    #[error("text is {len} bytes, maximum is {max}")]
    TooLong {
        /// Length of the offending input, in bytes.
        len: usize,
        /// Maximum accepted length, in bytes.
        max: usize,
    },
}

/// Text that is guaranteed to hold at least one non-whitespace character and
/// to be bounded in length.
///
/// Parsed once at a boundary so that the rest of the system never has to ask
/// whether a title, prompt, or identifier is blank.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct NonEmptyText(String);

impl NonEmptyText {
    /// Maximum accepted length in bytes, generous enough for a long feedback
    /// prompt but small enough to keep a runaway paste out of the domain.
    pub const MAX_LEN: usize = 64 * 1024;

    /// Parse free text into the domain, trimming the edges.
    pub fn parse(raw: &str) -> Result<Self, TextError> {
        if raw.len() > Self::MAX_LEN {
            return Err(TextError::TooLong {
                len: raw.len(),
                max: Self::MAX_LEN,
            });
        }
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(TextError::Empty);
        }
        Ok(Self(trimmed.to_owned()))
    }

    /// Borrow the contents.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Render the contents on one line, for list rows and status bars.
    #[must_use]
    pub fn single_line(&self) -> String {
        self.0.split_whitespace().collect::<Vec<_>>().join(" ")
    }
}

impl fmt::Display for NonEmptyText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for NonEmptyText {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for NonEmptyText {
    type Error = TextError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

impl<'de> Deserialize<'de> for NonEmptyText {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty_and_whitespace_only_input() {
        assert_eq!(NonEmptyText::parse(""), Err(TextError::Empty));
        assert_eq!(NonEmptyText::parse("   \t\n"), Err(TextError::Empty));
    }

    #[test]
    fn trims_surrounding_whitespace() {
        let text = NonEmptyText::parse("  fix the memory leak \n").expect("non-empty");
        assert_eq!(text.as_str(), "fix the memory leak");
    }

    #[test]
    fn preserves_interior_whitespace_and_newlines() {
        let text = NonEmptyText::parse("line one\nline two").expect("non-empty");
        assert_eq!(text.as_str(), "line one\nline two");
    }

    #[test]
    fn rejects_input_longer_than_the_limit() {
        let long = "x".repeat(NonEmptyText::MAX_LEN + 1);
        assert_eq!(
            NonEmptyText::parse(&long),
            Err(TextError::TooLong {
                len: NonEmptyText::MAX_LEN + 1,
                max: NonEmptyText::MAX_LEN
            })
        );
    }

    #[test]
    fn displays_as_its_contents() {
        let text = NonEmptyText::parse("DEV-891").expect("non-empty");
        assert_eq!(text.to_string(), "DEV-891");
    }

    #[test]
    fn single_line_collapses_newlines_for_display() {
        let text = NonEmptyText::parse("a\nb\nc").expect("non-empty");
        assert_eq!(text.single_line(), "a b c");
    }
}
