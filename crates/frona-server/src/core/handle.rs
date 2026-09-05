//! Per-scope-unique human identifier. Validated on construction so the type
//! makes invalid handles unrepresentable.
//!
//! - HTTP / DB / runtime input: [`Handle::try_new`] (trims + lowercases).
//! - Compile-time literals: [`handle!`] macro / [`Handle::const_validated`]
//!   (typos fail `cargo check`).
//!
//! Grammar: 2..=32 bytes, starts with `a-z`, body is `a-z 0-9 - _`.
//! Backed by [`SmolStr`] — every valid handle inlines (≤22 bytes).

use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use smol_str::SmolStr;
use std::fmt;
use surrealdb::types::{Error as SurrealError, Kind, SurrealValue, Value};

use crate::core::error::AppError;

pub const fn is_valid_handle_bytes(s: &[u8]) -> bool {
    if s.len() < 2 || s.len() > 32 {
        return false;
    }
    if !s[0].is_ascii_lowercase() {
        return false;
    }
    let mut i = 0;
    while i < s.len() {
        let b = s[i];
        let ok = b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_';
        if !ok {
            return false;
        }
        i += 1;
    }
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Handle(SmolStr);

impl Handle {
    /// Trims + lowercases ASCII before validating.
    pub fn try_new(raw: impl AsRef<str>) -> Result<Self, AppError> {
        let cleaned = raw.as_ref().trim().to_ascii_lowercase();
        if !is_valid_handle_bytes(cleaned.as_bytes()) {
            return Err(AppError::Validation(format!(
                "handle '{cleaned}' is invalid: must be 2-32 chars, start with a lowercase letter, and contain only a-z, 0-9, hyphens, underscores"
            )));
        }
        Ok(Self(SmolStr::new(cleaned)))
    }

    /// Panics at compile time on invalid input. No sanitization — literal
    /// must already be lowercase and trimmed.
    pub const fn const_validated(s: &'static str) -> Self {
        assert!(
            is_valid_handle_bytes(s.as_bytes()),
            "invalid Handle literal: must be 2-32 ASCII lowercase chars, start with a letter, contain only a-z/0-9/-/_"
        );
        Self(SmolStr::new_inline(s))
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    pub fn into_string(self) -> String {
        self.0.into()
    }
}

impl AsRef<str> for Handle {
    fn as_ref(&self) -> &str {
        self.0.as_str()
    }
}

impl std::borrow::Borrow<str> for Handle {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for Handle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0.as_str())
    }
}

impl PartialEq<&str> for Handle {
    fn eq(&self, other: &&str) -> bool {
        self.0.as_str() == *other
    }
}

impl PartialEq<str> for Handle {
    fn eq(&self, other: &str) -> bool {
        self.0.as_str() == other
    }
}

impl PartialEq<String> for Handle {
    fn eq(&self, other: &String) -> bool {
        self.0.as_str() == other.as_str()
    }
}

impl Serialize for Handle {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.0.as_str().serialize(s)
    }
}

impl<'de> Deserialize<'de> for Handle {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        Handle::try_new(raw).map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for Handle {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Handle".into()
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let mut schema = generator.subschema_for::<String>();
        let object = schema.ensure_object();
        object.insert("minLength".to_string(), 2.into());
        object.insert("maxLength".to_string(), 32.into());
        object.insert("pattern".to_string(), "^[a-z][a-z0-9_-]{1,31}$".into());
        schema
    }
}

impl SurrealValue for Handle {
    fn kind_of() -> Kind {
        String::kind_of()
    }

    fn is_value(value: &Value) -> bool {
        String::is_value(value)
    }

    fn into_value(self) -> Value {
        self.into_string().into_value()
    }

    fn from_value(value: Value) -> Result<Self, SurrealError> {
        let s = String::from_value(value)?;
        Handle::try_new(s).map_err(|e| SurrealError::thrown(format!("invalid Handle: {e}")))
    }
}

/// Build a `Handle` from a string literal; invalid input fails `cargo check`.
/// User input must go through [`Handle::try_new`] for validation errors.
#[macro_export]
macro_rules! handle {
    ($s:literal) => {{
        const __FRONA_HANDLE: $crate::core::Handle = $crate::core::Handle::const_validated($s);
        __FRONA_HANDLE
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_valid_handles() {
        assert!(Handle::try_new("ab").is_ok());
        assert!(Handle::try_new("alice").is_ok());
        assert!(Handle::try_new("alice-bob_2").is_ok());
        assert!(Handle::try_new("a".repeat(32)).is_ok());
    }

    #[test]
    fn sanitizes_trim_and_lowercase() {
        assert_eq!(Handle::try_new("  Alice  ").unwrap().as_ref(), "alice");
    }

    #[test]
    fn rejects_invalid() {
        assert!(Handle::try_new("a").is_err(), "too short");
        assert!(Handle::try_new("x".repeat(33)).is_err(), "too long");
        assert!(Handle::try_new("9alice").is_err(), "must start with letter");
        assert!(Handle::try_new("alice!").is_err(), "bad char");
        assert!(Handle::try_new("alice.bob").is_err(), "bad char");
        assert!(Handle::try_new("alice/bob").is_err(), "bad char");
    }

    #[test]
    fn macro_constructs_valid_handle() {
        const SYSTEM: Handle = handle!("system");
        assert_eq!(SYSTEM.as_str(), "system");
    }

    #[test]
    fn const_validated_works_in_const_context() {
        const _: Handle = Handle::const_validated("developer");
        const _LIST: &[Handle] = &[
            Handle::const_validated("developer"),
            Handle::const_validated("system"),
        ];
    }
}
