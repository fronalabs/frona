use crate::tools::description;
use crate::{Error, RecallProjection};
use frona_harness::{ToolRegistry, tool};
use schemars::JsonSchema;
use serde::Deserialize;
use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

#[derive(Deserialize, JsonSchema)]
struct RecallArguments {
    /// A prompt-local recall call ID such as T1.
    call_id: String,
}

#[derive(Deserialize, JsonSchema)]
struct EvidenceArguments {
    /// Prompt-local Agent message handle, such as m2.
    message_id: String,
    /// Factual claim or phrase to search for in qualified executions.
    query: String,
}

pub(super) fn registry(
    recall: RecallProjection,
    recall_lookups: Arc<AtomicUsize>,
    evidence_lookups: Arc<AtomicUsize>,
    messages: HashMap<String, String>,
    max_lookups: usize,
    searched_messages: Arc<Mutex<HashSet<String>>>,
) -> Result<ToolRegistry<'static>, Error> {
    let evidence = recall.evidence.clone();
    let evidence_allowance =
        crate::tools::allowance::ResearchAllowance::with_counter(max_lookups, evidence_lookups);
    let recall_tool = tool("read_recall_result", description(include_str!("../../resources/tools/read_recall_result.md")), move |args: RecallArguments| {
        let result = recall.read_result(&args.call_id);
        if result.is_some() { recall_lookups.fetch_add(1, Ordering::Relaxed); }
        async move { Ok(result.unwrap_or_else(|| "Unknown recall call_id. Only T-ids shown in this extraction window are readable.".into())) }
    }).map_err(|e| Error::Internal(e.to_string()))?;
    let evidence_tool = tool(
        "search_tool_evidence",
        description(include_str!(
            "../../resources/tools/search_tool_evidence.md"
        )),
        move |args: EvidenceArguments| {
            let result = if !evidence_allowance.try_acquire() {
                "The extraction evidence lookup budget is exhausted.".into()
            } else if let Some(message_id) = messages.get(&args.message_id) {
                searched_messages
                    .lock()
                    .expect("searched evidence messages poisoned")
                    .insert(args.message_id.clone());
                let handles = messages
                    .iter()
                    .map(|(handle, id)| (id.clone(), handle.clone()))
                    .collect();
                evidence.search_for_message_with_handles(
                    &args.message_id,
                    message_id,
                    &args.query,
                    evidence.result_token_cap,
                    &handles,
                )
            } else {
                "Unknown Agent message_id.".into()
            };
            async move { Ok(result) }
        },
    )
    .map_err(|e| Error::Internal(e.to_string()))?;
    let mut registry = ToolRegistry::default();
    registry
        .register(recall_tool)
        .map_err(|e| Error::Internal(e.to_string()))?;
    registry
        .register(evidence_tool)
        .map_err(|e| Error::Internal(e.to_string()))?;
    Ok(registry)
}
