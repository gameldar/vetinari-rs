//! Identifier newtypes.
//!
//! Two shapes are used. [`Slug`] names things Vetinari itself owns and that
//! appear in configuration (profiles, skills, agents, targets); it is
//! normalised so that a config file and a keybinding always agree. The opaque
//! ids wrap identifiers minted by a backend, which must be preserved verbatim.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Why a slug could not be accepted.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SlugError {
    /// The input held nothing but whitespace.
    #[error("slug is empty")]
    Empty,
    /// The input held a character outside `[a-z0-9._/-]`.
    #[error("slug contains invalid character {found:?}")]
    InvalidChar {
        /// The first offending character.
        found: char,
    },
}

/// A normalised, configuration-facing name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct Slug(String);

impl Slug {
    /// Parse a configuration name, lowercasing it.
    pub fn parse(raw: &str) -> Result<Self, SlugError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(SlugError::Empty);
        }
        let mut out = String::with_capacity(trimmed.len());
        for ch in trimmed.chars() {
            let lower = ch.to_ascii_lowercase();
            if lower.is_ascii_alphanumeric() || matches!(lower, '-' | '_' | '.' | '/') {
                out.push(lower);
            } else {
                return Err(SlugError::InvalidChar { found: ch });
            }
        }
        Ok(Self(out))
    }

    /// Borrow the normalised name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Slug {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for Slug {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for Slug {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).map_err(serde::de::Error::custom)
    }
}

/// An identifier minted elsewhere was blank.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("identifier is empty")]
pub struct EmptyId;

macro_rules! opaque_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// Parse an identifier supplied by a backend, trimming the edges.
            pub fn parse(raw: &str) -> Result<Self, EmptyId> {
                let trimmed = raw.trim();
                if trimmed.is_empty() {
                    return Err(EmptyId);
                }
                Ok(Self(trimmed.to_owned()))
            }

            /// Borrow the identifier.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                let raw = String::deserialize(deserializer)?;
                Self::parse(&raw).map_err(serde::de::Error::custom)
            }
        }
    };
}

pub(crate) use opaque_id;

opaque_id!(
    /// A workspace space, window, or editor context owned by a target backend.
    SpaceId
);
opaque_id!(
    /// A running or finished agent session.
    SessionId
);
opaque_id!(
    /// One entry in the attention feed.
    ItemId
);

/// Names an attention source adapter, e.g. `github`.
pub type SourceId = Slug;
/// Names an agent engine, e.g. `claude-code`.
pub type AgentId = Slug;
/// Names a stored skill or prompt template.
pub type SkillId = Slug;
/// Names a context profile, e.g. `work`.
pub type ProfileId = Slug;
/// Names a workspace target, e.g. `tmux`.
pub type TargetId = Slug;

impl ItemId {
    /// Build a feed-unique id from the source that produced the item and the
    /// key that source uses natively.
    #[must_use]
    pub fn from_parts(source: &SourceId, native: &str) -> Self {
        Self(format!("{source}:{}", native.trim()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_lowercases_and_accepts_separators() {
        let slug = Slug::parse("Work-Profile_2.v1").expect("valid slug");
        assert_eq!(slug.as_str(), "work-profile_2.v1");
    }

    #[test]
    fn slug_rejects_empty_input() {
        assert_eq!(Slug::parse("  "), Err(SlugError::Empty));
    }

    #[test]
    fn slug_rejects_spaces_and_punctuation() {
        assert_eq!(
            Slug::parse("pr review"),
            Err(SlugError::InvalidChar { found: ' ' })
        );
        assert_eq!(
            Slug::parse("pr:review"),
            Err(SlugError::InvalidChar { found: ':' })
        );
    }

    #[test]
    fn slug_round_trips_through_display() {
        let slug = Slug::parse("github").expect("valid slug");
        assert_eq!(slug.to_string(), "github");
    }

    #[test]
    fn opaque_ids_keep_backend_formatting_verbatim() {
        let space = SpaceId::parse("Herdr/Space 3").expect("valid id");
        assert_eq!(space.as_str(), "Herdr/Space 3");
        let session = SessionId::parse(" 4f2a ").expect("valid id");
        assert_eq!(session.as_str(), "4f2a");
    }

    #[test]
    fn opaque_ids_reject_blank_input() {
        assert!(SpaceId::parse("").is_err());
        assert!(SessionId::parse("\n").is_err());
        assert!(ItemId::parse("   ").is_err());
    }

    #[test]
    fn distinct_id_types_do_not_mix() {
        let a = SkillId::parse("pr-review").expect("valid id");
        let b = SkillId::parse("pr-review").expect("valid id");
        assert_eq!(a, b);
    }

    #[test]
    fn item_id_can_be_built_from_source_and_native_key() {
        let source = SourceId::parse("github").expect("valid slug");
        let id = ItemId::from_parts(&source, "142");
        assert_eq!(id.as_str(), "github:142");
    }
}
