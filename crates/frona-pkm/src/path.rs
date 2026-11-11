pub fn normalize_path(raw: &str) -> Option<String> {
    let cleaned = raw.trim().trim_start_matches('/').trim_end_matches(".md");
    let segments: Vec<String> = cleaned
        .split('/')
        .map(slugify)
        .filter(|s| !s.is_empty() && s != "..")
        .collect();
    if segments.is_empty() {
        return None;
    }
    Some(segments.join("/"))
}

/// A single path segment → kebab-case ASCII slug (≤60 chars).
///
/// Non-ASCII is **transliterated**, not discarded. The slug is a page's identity key, its
/// location on disk, and the target of every `[[wikilink]]` pointing at it, so it stays
/// ASCII by design: a Unicode path would be compared as a string against a filename the
/// filesystem may have normalised differently (NFC vs NFD), which would silently fork one
/// page into two.
///
/// Discarding was the previous behaviour and it was worse than it looked. Every
/// non-ASCII character became a space, so `"José García"` slugged to `"jos-garc"` and
/// `"田中"` slugged to **nothing at all** - `normalize_path` then returned `None` and the
/// caller skipped the page entirely. Any entity named in a non-Latin script was dropped
/// without a trace; it only went unnoticed because the model tends to romanise.
fn slugify(s: &str) -> String {
    // Unrepresentable characters become a separator rather than deunicode's default
    // `[?]` marker, which would otherwise survive as the literal word "tofu" glyphs.
    let ascii = deunicode::deunicode_with_tofu(s, " ");
    let lowered: String = ascii
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect();
    let words: Vec<&str> = lowered.split_whitespace().collect();
    words
        .join("-")
        .chars()
        .take(60)
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}
