//! Historical Anthropic input excludes cache reads/writes, but Rig's total
//! includes both. Recover total prompt input only when the recorded counts
//! support it, without guessing the wire protocol from a connection handle.
//! Prices and pricing snapshots are deliberately untouched.

use frona_derive::migration;

#[migration("2026-10-21T00:00:00Z")]
fn normalize_inference_input_tokens() -> &'static str {
    // Already-total-shaped rows are no-ops. For disjoint counts, the recovered
    // input must cover fresh input plus cache reads; the remainder accounts for
    // writes without inventing their missing historical breakdown. Malformed or
    // missing totals stay unchanged and are identified by usage rollups.
    // After a successful update total-output equals input, so reruns are no-ops.
    // SurrealDB may evaluate SET before WHERE, so both arithmetic expressions
    // need a type guard; a missing total must not abort the entire migration.
    "UPDATE inference_usage SET input_tokens = IF type::is_int(total_tokens)
         AND type::is_int(output_tokens) { total_tokens - output_tokens } ELSE { input_tokens }
     WHERE IF type::is_int(total_tokens) AND type::is_int(output_tokens)
           AND type::is_int(input_tokens) AND type::is_int(cached_input_tokens) {
         output_tokens >= 0 AND input_tokens >= 0 AND cached_input_tokens >= 0
         AND total_tokens >= output_tokens
         AND total_tokens - output_tokens > input_tokens
         AND total_tokens - output_tokens - input_tokens >= cached_input_tokens
     } ELSE { false };"
}
