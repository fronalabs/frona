//! Use an existing Rig client with Frona's loops. Requires OPENAI_API_KEY.
use frona_harness::{Harness, Model};
use rig_core::providers::openai::OpenAI;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = OpenAI::from_env()?;
    let model = Model::from_rig(client.responses("gpt-5-mini"))?;
    let answer = Harness::new(model)
        .run("Explain Rust ownership in one sentence")
        .await?;
    println!("{answer}");
    Ok(())
}
