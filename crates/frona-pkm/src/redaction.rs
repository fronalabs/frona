//! PKM field-selection policies. Shared text scanning and JSON traversal live in frona-text.
use frona_text::redaction::{FieldAction, redact_json as project};
use serde_json::Value;

pub(crate) fn sanitize_arguments(value: &Value) -> Value {
    project(value, |key| {
        if Projection::Ingest.sensitive(key) {
            FieldAction::Omit
        } else {
            FieldAction::Keep
        }
    })
}

pub(crate) fn redact_json(value: &Value) -> Value {
    project(value, |key| {
        if Projection::Playbook.sensitive(key) {
            FieldAction::Redact
        } else {
            FieldAction::Keep
        }
    })
}

#[derive(Clone, Copy)]
enum Projection {
    Ingest,
    Playbook,
}
impl Projection {
    fn sensitive(self, key: &str) -> bool {
        match self {
            Self::Ingest => matches!(
                key.to_ascii_lowercase().as_str(),
                "authorization"
                    | "proxy-authorization"
                    | "cookie"
                    | "set-cookie"
                    | "token"
                    | "access_token"
                    | "refresh_token"
                    | "api_key"
                    | "apikey"
                    | "password"
                    | "secret"
                    | "binary"
                    | "bytes"
                    | "base64"
                    | "image_data"
                    | "audio_data"
            ),
            Self::Playbook => {
                let key = key.to_ascii_lowercase().replace(['-', '_'], "");
                [
                    "authorization",
                    "password",
                    "passwd",
                    "secret",
                    "token",
                    "apikey",
                    "accesskey",
                    "privatekey",
                    "clientsecret",
                    "credential",
                    "cookie",
                ]
                .iter()
                .any(|part| key.contains(part))
            }
        }
    }
}

#[cfg(test)]
mod tests;
