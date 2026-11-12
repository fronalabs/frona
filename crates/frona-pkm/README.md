# frona-pkm

Compose personal knowledge memory workflows from reusable reasoning phases.
The library takes caller-scoped snapshots and produces uncommitted outputs. It
requires no Frona server or SurrealDB connection.

## Install

```toml
[dependencies]
frona-pkm = "0.1"
frona-harness = "0.1"
```

## Extract a transcript

Supply a configured harness and message-anchored evidence. This example accepts an
existing harness so the application chooses its provider and credentials.

```no_run
use frona_harness::Harness;
use frona_pkm::{
    TranscriptEvidenceKind, TranscriptEvidenceSource,
    ingest::{Ingest, IngestInput},
};

async fn extract(harness: Harness<'static>) -> Result<(), frona_pkm::Error> {
    let text = "I prefer tea to coffee.";
    let batch = Ingest::new(harness).run(IngestInput {
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
    }).await?;
    // Validate current revisions and persist the batch in your application's transaction.
    let _ = batch;
    Ok(())
}
```

## Choose the phases your workflow needs

| Phase | Responsibility |
|---|---|
| `ingest::Ingest` | Extract grounded memories and entity contributions |
| `classify::Classify` | Classify entities and propose vocabulary |
| `resolve::Resolve` | Investigate entity identity and propose merges |
| `reconcile::Reconcile` | Reconcile entity knowledge and memories |
| `assemble::Assemble` | Adjudicate vocabulary and assemble a consistent proposal |
| `page::PageAuthor` | Author Markdown wiki projections |
| `playbook::resolve::PlaybookResolve` | Resolve procedural knowledge candidates |
| `playbook::author::PlaybookAuthor` | Author playbook content |
| `writeback::Writeback` | Interpret page edits as proposed memory operations |

Each phase has embedded, replaceable prompts. `run_with` lets the host configure
harness execution, including shared budgets, cancellation, and observers.
Prompt defaults can supply application context. There are no optional Cargo features.

## Persistence and evidence

The caller owns authorization, snapshot consistency, durable models, database
transactions, scheduling, and watermarks. Outputs are proposals; successful model
execution alone does not commit domain progress. An error must not advance durable
progress. Revalidate replayed checkpoint material against current state.

User statements can ground user-sourced memories. Agent prose alone is not evidence
of truth: supply the relevant provenance and admissible supporting tool evidence.
Knowledge entities are semantic records; wiki pages are projections of those records.

The crate also exposes ontology reasoning and graph snapshots. It uses the published
`frona-ontologies` extractor; applications supply the ontology catalogue and scoped
knowledge required by their chosen phases.

## Examples

The package includes `ingest`, `consolidate`, `playbook`, and `page_edit` examples.
They use live OpenAI clients and require `OPENAI_API_KEY`. The `consolidate`
example also needs `PKM_ONTOLOGY_DIR`, pointing to your ontology files. From a source checkout:

```sh
cargo run -p frona-pkm --example ingest
```

See the [API reference](https://docs.rs/frona-pkm) for phase inputs, outputs,
checkpoint hooks, and candidate retrieval interfaces.

## License

MIT. See `LICENSE`. Dependencies and externally supplied ontology data have their own licenses.
