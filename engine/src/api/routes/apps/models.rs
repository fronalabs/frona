use serde::Deserialize;

#[derive(Deserialize)]
pub(super) struct ServiceActionRequest {
    pub(super) chat_id: String,
}
