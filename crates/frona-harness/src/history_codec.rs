//! Versioned JSONL encoding and decoding; no file or execution policy.
use std::ops::Range;

use super::{HistoryRecord, HistoryReplay, replay_history};

/// Stable failure classes; complete-line corruption never returns a usable prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryDecodeErrorKind {
    Corruption,
    UnsupportedVersion,
    Format,
    InvalidState,
    MissingOpened,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("history {kind:?} at record {seq}, byte {byte:?}: {message}")]
pub struct HistoryDecodeError {
    pub kind: HistoryDecodeErrorKind,
    /// Expected sequence number, even if the record's own sequence is corrupt.
    pub seq: u64,
    /// Start of the offending complete line; absent for typed-record replay.
    pub byte: Option<usize>,
    pub message: String,
}

impl HistoryDecodeError {
    pub(super) fn new(kind: HistoryDecodeErrorKind, seq: u64, message: impl ToString) -> Self {
        Self {
            kind,
            seq,
            byte: None,
            message: message.to_string(),
        }
    }
}

/// A reconstructed prefix and an explicitly excluded, non-LF-terminated suffix.
/// Callers decide how to handle that suffix; decoding never repairs storage.
#[derive(Debug)]
pub struct DecodedHistory {
    pub replay: HistoryReplay,
    pub incomplete_tail: Option<Range<usize>>,
}

/// Encode records with an LF after every record. This is a codec, not a commit or
/// a semantic validator; use `replay_history` to validate a typed record sequence.
pub fn encode_jsonl(records: &[HistoryRecord]) -> Result<Vec<u8>, serde_json::Error> {
    let mut bytes = Vec::new();
    for record in records {
        serde_json::to_writer(&mut bytes, record)?;
        bytes.push(b'\n');
    }
    Ok(bytes)
}

/// Decode complete lines and replay all of them, without models, tools, callbacks,
/// filesystem access, or budget restoration. A nonterminated tail is never applied,
/// including when it contains syntactically valid JSON.
pub fn decode_jsonl(bytes: &[u8]) -> Result<DecodedHistory, HistoryDecodeError> {
    let end = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |p| p + 1);
    let incomplete_tail = (end < bytes.len()).then_some(end..bytes.len());
    let mut records = Vec::new();
    let mut offsets = Vec::new();
    let mut offset = 0;
    for line in bytes[..end].split_inclusive(|b| *b == b'\n') {
        let seq = records.len() as u64;
        let error = |kind, message| HistoryDecodeError {
            kind,
            seq,
            byte: Some(offset),
            message,
        };
        let mut value: serde_json::Value = serde_json::from_slice(line)
            .map_err(|e| error(HistoryDecodeErrorKind::Corruption, e.to_string()))?;
        let version = value
            .get("version")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| {
                error(
                    HistoryDecodeErrorKind::Format,
                    "missing or invalid version".into(),
                )
            })?;
        if version != 1 && version != 2 {
            return Err(error(
                HistoryDecodeErrorKind::UnsupportedVersion,
                format!("unsupported version {version}"),
            ));
        }
        let record = if version == 1 {
            // Legacy conversion needs a Value, but must not collapse duplicate
            // structural fields before the typed history decoder sees them.
            value = serde_json::from_slice::<UniqueValue>(line)
                .map_err(|e| error(HistoryDecodeErrorKind::Format, e.to_string()))?
                .0;
            crate::inference::history_compat::upgrade_record(&mut value)
                .map_err(|e| error(HistoryDecodeErrorKind::Format, e.to_string()))?;
            serde_json::from_value(value)
                .map_err(|e| error(HistoryDecodeErrorKind::Format, e.to_string()))?
        } else {
            serde_json::from_slice(line)
                .map_err(|e| error(HistoryDecodeErrorKind::Format, e.to_string()))?
        };
        records.push(record);
        offsets.push(offset);
        offset += line.len();
    }
    let replay = replay_history(&records).map_err(|mut error| {
        error.byte = usize::try_from(error.seq)
            .ok()
            .and_then(|seq| offsets.get(seq).copied());
        error
    })?;
    Ok(DecodedHistory {
        replay,
        incomplete_tail,
    })
}

/// Reject ambiguous legacy JSON at every depth before migration. Application
/// payloads are preserved as values; malformed duplicate keys are not repaired.
struct UniqueValue(serde_json::Value);

impl<'de> serde::Deserialize<'de> for UniqueValue {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = UniqueValue;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a JSON value without duplicate object keys")
            }

            fn visit_bool<E: serde::de::Error>(self, value: bool) -> Result<Self::Value, E> {
                Ok(UniqueValue(value.into()))
            }
            fn visit_i64<E: serde::de::Error>(self, value: i64) -> Result<Self::Value, E> {
                Ok(UniqueValue(value.into()))
            }
            fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<Self::Value, E> {
                Ok(UniqueValue(value.into()))
            }
            fn visit_f64<E: serde::de::Error>(self, value: f64) -> Result<Self::Value, E> {
                serde_json::Number::from_f64(value)
                    .map(|number| UniqueValue(serde_json::Value::Number(number)))
                    .ok_or_else(|| E::custom("invalid JSON number"))
            }
            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
                Ok(UniqueValue(value.into()))
            }
            fn visit_string<E: serde::de::Error>(self, value: String) -> Result<Self::Value, E> {
                Ok(UniqueValue(value.into()))
            }
            fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(UniqueValue(serde_json::Value::Null))
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<Self::Value, A::Error> {
                let mut values = Vec::new();
                while let Some(UniqueValue(value)) = seq.next_element()? {
                    values.push(value);
                }
                Ok(UniqueValue(serde_json::Value::Array(values)))
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Self::Value, A::Error> {
                let mut values = serde_json::Map::new();
                while let Some(key) = map.next_key::<String>()? {
                    if values.contains_key(&key) {
                        return Err(serde::de::Error::custom(format!("duplicate field `{key}`")));
                    }
                    let UniqueValue(value) = map.next_value()?;
                    values.insert(key, value);
                }
                Ok(UniqueValue(serde_json::Value::Object(values)))
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}
