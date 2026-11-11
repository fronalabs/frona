/// The unified diff a human edit yields - a `similar` line diff of the current head
/// vs the incoming file, with the machine-owned `## History` stripped from both
/// (a user editing/deleting a History line must not read as retracting a fact).
/// This is what the LLM write-back consumes to decide memory ops.
pub fn writeback_diff(head: &str, incoming: &str) -> String {
    fn strip_history(s: &str) -> &str {
        match s.find("\n## History") {
            Some(i) => &s[..i],
            None => s,
        }
    }
    similar::TextDiff::from_lines(strip_history(head), strip_history(incoming))
        .unified_diff()
        .context_radius(3)
        .header("head", "user-edited")
        .to_string()
}
