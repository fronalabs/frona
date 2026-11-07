//! Pure version-one JSONL encoding and decoding; no file or execution policy.
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
        let value: serde_json::Value = serde_json::from_slice(line)
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
        if version != 1 {
            return Err(error(
                HistoryDecodeErrorKind::UnsupportedVersion,
                format!("unsupported version {version}"),
            ));
        }
        let record = serde_json::from_slice(line)
            .map_err(|e| error(HistoryDecodeErrorKind::Format, e.to_string()))?;
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
