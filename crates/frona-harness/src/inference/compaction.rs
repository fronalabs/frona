//! Pure pieces shared by portable and persistent compaction adapters.

use rig_core::completion::Message;

/// Keep the protected suffix, then include older whole groups while the target
/// allows. Group costs may be estimated or calibrated by the caller.
///
/// # Panics
/// Panics if `protected` exceeds `costs.len()`; it is the first protected group,
/// or the length when no group must be retained.
pub fn retained_suffix_start(
    costs: &[usize],
    protected: usize,
    fixed_tokens: usize,
    summary_tokens: usize,
    target: usize,
) -> usize {
    let mut start = protected;
    let mut running = costs[protected..]
        .iter()
        .fold(0usize, |n, cost| n.saturating_add(*cost));
    for index in (0..protected).rev() {
        if running
            .saturating_add(costs[index])
            .saturating_add(fixed_tokens)
            .saturating_add(summary_tokens)
            > target
        {
            break;
        }
        running = running.saturating_add(costs[index]);
        start = index;
    }
    start
}

/// Render the fixed leading summary and an already captured context. Source
/// collection and snapshot-signature persistence belong to the runtime adapter.
pub fn summary_message(summary: &str, context: Option<&str>) -> Message {
    let content = match context.filter(|s| !s.is_empty()) {
        Some(context) => format!("{summary}\n\n{context}"),
        None => summary.to_owned(),
    };
    Message::user(format!(
        "<conversation_summary>\n{content}\n</conversation_summary>"
    ))
}
