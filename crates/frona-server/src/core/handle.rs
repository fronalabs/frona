//! Per-scope-unique human identifier. Validated on construction so the type
//! makes invalid handles unrepresentable.
//!
//! - HTTP / DB / runtime input: [`Handle::try_new`] (trims + lowercases).
//! - Compile-time literals: [`handle!`] macro / [`Handle::const_validated`]
//!   (typos fail `cargo check`).
//!
//! Grammar: 2..=32 bytes, starts with `a-z`, body is `a-z 0-9 - _`.
//! Wraps the portable identifier so native database encoding stays server-owned.

use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use surrealdb::types::{Error as SurrealError, Kind, SurrealValue, Value};

use crate::core::error::AppError;

pub const fn is_valid_handle_bytes(s: &[u8]) -> bool {
    frona_harness::handle::is_valid_handle_bytes(s)
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Handle(frona_harness::handle::Handle);

impl Handle {
    /// Trims + lowercases ASCII before validating.
    pub fn try_new(raw: impl AsRef<str>) -> Result<Self, AppError> {
        frona_harness::handle::Handle::try_new(raw)
            .map(Self)
            .map_err(|error| AppError::Validation(error.to_string()))
    }

    /// Construct a validated literal while preserving the native stored type.
    pub const fn const_validated(s: &'static str) -> Self {
        Self(frona_harness::handle::Handle::const_validated(s))
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    pub fn into_string(self) -> String {
        self.0.into_string()
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
        frona_harness::handle::Handle::schema_name()
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        frona_harness::handle::Handle::json_schema(generator)
    }
}

impl From<Handle> for frona_harness::handle::Handle {
    fn from(value: Handle) -> Self {
        value.0
    }
}

impl From<frona_harness::handle::Handle> for Handle {
    fn from(value: frona_harness::handle::Handle) -> Self {
        Self(value)
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
