/// The identifying tail of a CURIE or IRI - the part a human reads as the name.
pub(super) fn local_name(id: &str) -> &str {
    id.rsplit(['#', '/', ':']).next().unwrap_or(id)
}

/// Split CamelCase so `DatabaseDesign` compares as "database design".
pub(super) fn decamel(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for (i, c) in s.char_indices() {
        if i > 0 && c.is_uppercase() {
            out.push(' ');
        }
        out.push(c);
    }
    out
}

/// A term name reduced to comparable words: a trailing `_Q<id>` disambiguator
/// dropped, separators collapsed to spaces, lowercased.
/// `Database_Interface_Q1172367` → `database interface`.
///
/// The `_u0028_`-style escapes some vocabularies encode punctuation as are left
/// alone deliberately: they sit *between* separators, so they fall out as their own
/// token and never split a real word.
pub(super) fn normalize(name: &str) -> String {
    let trimmed = match name.rfind("_Q") {
        Some(i) if name.len() > i + 2 && name[i + 2..].bytes().all(|c| c.is_ascii_digit()) => {
            &name[..i]
        }
        _ => name,
    };
    trimmed
        .replace(['_', '-'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Compare `needle` to `candidate` with the candidate's spaces ignored, so a query written
/// as one word can match a term whose words are separated.
///
/// `prefix_only` stops at the end of the needle instead of requiring both to run out.
/// Allocation-free on purpose: this runs once per needle per term, and the vocabulary is
/// 30k terms - squashing each candidate into a fresh `String` would allocate that many
/// times per search.
pub(super) fn squashed_match(needle: &str, candidate: &str, prefix_only: bool) -> bool {
    let mut c = candidate.chars().filter(|c| *c != ' ');
    let mut n = needle.chars();
    loop {
        match (n.next(), c.next()) {
            (None, None) => return true,
            (None, Some(_)) => return prefix_only,
            (Some(_), None) => return false,
            (Some(a), Some(b)) if a == b => continue,
            _ => return false,
        }
    }
}

/// How well `needle` matches a candidate name; **lower is better**, `None` = no match.
/// `squashed` is `needle` with its spaces removed, computed once by the caller.
///
/// The ordering is what makes the tool usable against a large vocabulary: with 30k
/// classes, "database" substring-matches thousands of terms, so an exact hit has to
/// outrank `abstract database` or it never survives the result cap.
///
/// The squashed tiers exist because a term's *name* is decameled into words before it is
/// compared (`ProgrammingLanguage` → `programming language`) while the query is not. A model
/// asking in the form our own prompts ask it to write terms in - `ProgrammingLanguage` -
/// therefore could not match a spaced candidate on any tier, and `kbpedia:ProgrammingLanguage`
/// was unreachable while `"programming language"` found it at rank 0. Two entities for the same
/// kind of entity got two different classes purely from how the model happened to phrase the
/// search.
pub(super) fn match_rank(needle: &str, squashed: &str, candidate: &str) -> Option<u8> {
    if candidate == needle || squashed_match(squashed, candidate, false) {
        Some(0)
    } else if candidate.starts_with(needle) || squashed_match(squashed, candidate, true) {
        Some(1)
    } else if candidate.split(' ').any(|w| w.starts_with(needle)) {
        Some(2)
    } else if candidate.contains(needle) {
        Some(3)
    } else {
        None
    }
}

/// A needle with its spaces removed - the form the squashed tiers compare against.
pub(super) fn squash(needle: &str) -> String {
    needle.chars().filter(|c| *c != ' ').collect()
}
