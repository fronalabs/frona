//! Extract a transcript without a Frona server or database. Requires OPENAI_API_KEY.
use frona_harness::{Harness, Model};
use frona_pkm::{
    TranscriptEvidenceKind, TranscriptEvidenceSource,
    ingest::{Ingest, IngestInput},
};
use rig_core::providers::openai::OpenAI;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = OpenAI::from_env()?;
    let ingest = Ingest::new(Harness::new(Model::from_rig(
        client.responses("gpt-5-mini"),
    )?));
    let text = "I prefer tea to coffee.";
    let batch = ingest
        .run(IngestInput {
            transcript: format!("[m1] User: {text}"),
            owner_name: "Casey".into(),
            evidence_sources: vec![TranscriptEvidenceSource {
                handle: "m1".into(),
                text: text.into(),
                kind: TranscriptEvidenceKind::UserMessage {
                    message_id: "message-1".into(),
                    chat_id: "conversation-1".into(),
                },
            }],
            ..Default::default()
        })
        .await?;
    // The application decides how to persist this batch and advance its progress.
    println!("{}", serde_json::to_string_pretty(&batch)?);
    Ok(())
}
