//! `NormalizedString` - alignment-tracked text normalisation.
//!
//! Holds an immutable `original` string plus a `normalized` form. Every byte
//! of `normalized` has a corresponding `(src_start, src_end)` byte range in
//! `original` - the source bytes "owned" by that normalised byte, INCLUDING
//! any source bytes that were absorbed by neighbouring transforms (e.g.
//! whitespace collapse, NFKC composition).
//!
//! This is the load-bearing property for Edit's "byte-identical outside the
//! edit" guarantee: when a needle matches normalised bytes `[a..b)`,
//! `splice_range_original(a..b)` returns the original-buffer range to
//! replace - including any absorbed neighbours that have no normalised byte
//! of their own.
//!
//! Spans are `(u32, u32)` per byte → 4 GiB file-size ceiling.

use unicode_casefold::UnicodeCaseFold;
use unicode_normalization_alignments::UnicodeNormalization;

/// A `(src_start, src_end)` byte range in the original buffer, stored
/// per-byte of the normalised buffer. `src_end` includes any source bytes
/// absorbed by neighbouring transforms.
type Span = (u32, u32);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedString {
    original: String,
    normalized: String,
    /// One entry per byte of `normalized`. All bytes belonging to the same
    /// normalised char share the same entry.
    spans: Vec<Span>,
}

impl NormalizedString {
    /// Build from a string with identity alignment. Each char's bytes
    /// share a span covering exactly that char's source range.
    pub fn from(s: &str) -> Self {
        let mut spans = Vec::with_capacity(s.len());
        for (b, c) in s.char_indices() {
            let span = (b as u32, (b + c.len_utf8()) as u32);
            for _ in 0..c.len_utf8() {
                spans.push(span);
            }
        }
        Self {
            original: s.to_string(),
            normalized: s.to_string(),
            spans,
        }
    }

    pub fn get(&self) -> &str {
        &self.normalized
    }

    pub fn get_original(&self) -> &str {
        &self.original
    }

    pub fn len(&self) -> usize {
        self.normalized.len()
    }

    pub fn len_original(&self) -> usize {
        self.original.len()
    }

    pub fn is_empty(&self) -> bool {
        self.normalized.is_empty()
    }

    /// Map a normalised byte range to the original-buffer byte range to
    /// splice over. The result includes any absorbed neighbouring source
    /// bytes (e.g. extra whitespace in a collapsed run, the combining
    /// accent absorbed into NFKC composition).
    ///
    /// Returns `None` if `range` is reversed or out of bounds.
    pub fn splice_range_original(
        &self,
        range: std::ops::Range<usize>,
    ) -> Option<std::ops::Range<usize>> {
        if range.start > range.end {
            return None;
        }
        if range.end > self.spans.len() {
            return None;
        }
        if range.start == range.end {
            let pos = if range.start < self.spans.len() {
                self.spans[range.start].0 as usize
            } else {
                self.original.len()
            };
            return Some(pos..pos);
        }
        let start = self.spans[range.start].0 as usize;
        let end = self.spans[range.end - 1].1 as usize;
        Some(start..end)
    }

    /// Spans for composed output cover the full source range of the absorbed
    /// source chars; spans for newly-inserted output chars (NFKC ligature
    /// decomposition) inherit from the previous emitted char.
    pub fn nfkc(&mut self) -> &mut Self {
        let mut new_normalized = String::with_capacity(self.normalized.len());
        let mut new_spans: Vec<Span> = Vec::with_capacity(self.spans.len());

        // Materialise the NFKC output so we can iterate the source in
        // parallel without borrow conflicts.
        let nfkc_output: Vec<(char, isize)> = self.normalized.nfkc().collect();

        let mut byte_pos = 0usize;
        let mut chars = self.normalized.chars();

        for (out_char, diff) in nfkc_output {
            let (cur_start, cur_end) = if diff == 0 {
                let c = chars.next().expect("source out of sync (diff=0)");
                let start = byte_pos;
                byte_pos += c.len_utf8();
                (start, byte_pos)
            } else if diff < 0 {
                // Composing: 1 + |diff| source chars → 1 output char.
                let n = 1 + (-diff) as usize;
                let start = byte_pos;
                for _ in 0..n {
                    let c = chars.next().expect("source out of sync (diff<0)");
                    byte_pos += c.len_utf8();
                }
                (start, byte_pos)
            } else {
                // Insertion (diff > 0): no source consumed.
                (byte_pos, byte_pos)
            };

            let span = if cur_end > cur_start {
                let mut min_start = self.spans[cur_start].0;
                let mut max_end = self.spans[cur_start].1;
                for &(start, end) in &self.spans[cur_start + 1..cur_end] {
                    min_start = min_start.min(start);
                    max_end = max_end.max(end);
                }
                (min_start, max_end)
            } else {
                new_spans.last().copied().unwrap_or((0, 0))
            };

            let mut buf = [0u8; 4];
            let s = out_char.encode_utf8(&mut buf);
            new_normalized.push_str(s);
            for _ in 0..s.len() {
                new_spans.push(span);
            }
        }

        self.normalized = new_normalized;
        self.spans = new_spans;
        self
    }

    /// Fold smart quotes (U+2018–U+201F) to ASCII `'` or `"`. Per-char
    /// 1:1 map; byte count may shrink (smart quote is 3 bytes, ASCII is 1).
    pub fn ascii_quotes(&mut self) -> &mut Self {
        self.map_chars(|c| match c {
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
            other => other,
        })
    }

    /// Fold Unicode dashes (U+2010–U+2015, U+2212) to ASCII `-`.
    pub fn ascii_dashes(&mut self) -> &mut Self {
        self.map_chars(|c| match c {
            '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}'
            | '\u{2212}' => '-',
            other => other,
        })
    }

    /// Fold non-ASCII spaces (NBSP, narrow NBSP, ideographic space, etc.)
    /// to ASCII `' '`.
    pub fn ascii_spaces(&mut self) -> &mut Self {
        self.map_chars(|c| match c {
            '\u{00A0}' | '\u{2002}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}' => ' ',
            other => other,
        })
    }

    /// Per-char 1:1 map. Allocates a new normalised buffer (byte count
    /// can change). Span for each emitted byte = span of the source char
    /// (no absorption - every char maps to exactly one output char).
    fn map_chars<F: Fn(char) -> char>(&mut self, f: F) -> &mut Self {
        let mut new_normalized = String::with_capacity(self.normalized.len());
        let mut new_spans: Vec<Span> = Vec::with_capacity(self.spans.len());

        let mut byte_pos = 0usize;
        for c in self.normalized.chars() {
            // All bytes of one char share the same span - pick the first.
            let span = self.spans[byte_pos];
            let mut buf = [0u8; 4];
            let s = f(c).encode_utf8(&mut buf);
            new_normalized.push_str(s);
            for _ in 0..s.len() {
                new_spans.push(span);
            }
            byte_pos += c.len_utf8();
        }

        self.normalized = new_normalized;
        self.spans = new_spans;
        self
    }

    /// Apply Unicode full, locale-independent case folding while retaining source
    /// alignment. Expanded output characters such as `ss` from `ß` inherit the source
    /// character's complete byte span.
    pub fn case_fold(&mut self) -> &mut Self {
        self.flat_map_chars(|c| c.case_fold())
    }

    /// Keep only characters accepted by `predicate`, preserving their original spans.
    pub fn retain_chars<F: Fn(char) -> bool>(&mut self, predicate: F) -> &mut Self {
        let mut normalized = String::with_capacity(self.normalized.len());
        let mut spans = Vec::with_capacity(self.spans.len());
        let mut byte_pos = 0usize;
        for c in self.normalized.chars() {
            let span = self.spans[byte_pos];
            if predicate(c) {
                normalized.push(c);
                spans.extend(std::iter::repeat_n(span, c.len_utf8()));
            }
            byte_pos += c.len_utf8();
        }
        self.normalized = normalized;
        self.spans = spans;
        self
    }

    fn flat_map_chars<I, F>(&mut self, f: F) -> &mut Self
    where
        I: Iterator<Item = char>,
        F: Fn(char) -> I,
    {
        let mut normalized = String::with_capacity(self.normalized.len());
        let mut spans = Vec::with_capacity(self.spans.len());
        let mut byte_pos = 0usize;
        for c in self.normalized.chars() {
            let span = self.spans[byte_pos];
            for mapped in f(c) {
                normalized.push(mapped);
                spans.extend(std::iter::repeat_n(span, mapped.len_utf8()));
            }
            byte_pos += c.len_utf8();
        }
        self.normalized = normalized;
        self.spans = spans;
        self
    }

    /// Collapse runs of whitespace (any `char::is_whitespace`) into a
    /// single ASCII `' '`. Absorbed source bytes extend the surviving
    /// space's span end - so a splice on the collapsed space covers the
    /// whole original run.
    pub fn collapse_whitespace_runs(&mut self) -> &mut Self {
        let mut new_normalized = String::with_capacity(self.normalized.len());
        let mut new_spans: Vec<Span> = Vec::with_capacity(self.spans.len());

        let mut prev_ws = false;
        let mut byte_pos = 0usize;
        // Track how many bytes we just emitted, so we know which trailing
        // span entries to extend when the next char is absorbed.
        let mut last_emitted_bytes = 0usize;

        for c in self.normalized.chars() {
            let span = self.spans[byte_pos];

            if c.is_whitespace() {
                if prev_ws {
                    // Absorbed. Extend the most-recently-emitted byte's
                    // span end to cover this absorbed char's source range.
                    if last_emitted_bytes > 0 {
                        let len = new_spans.len();
                        for entry in &mut new_spans[(len - last_emitted_bytes)..len] {
                            entry.1 = entry.1.max(span.1);
                        }
                    }
                    byte_pos += c.len_utf8();
                    continue;
                }
                prev_ws = true;
                new_normalized.push(' ');
                new_spans.push(span);
                last_emitted_bytes = 1;
            } else {
                prev_ws = false;
                let mut buf = [0u8; 4];
                let s = c.encode_utf8(&mut buf);
                new_normalized.push_str(s);
                for _ in 0..s.len() {
                    new_spans.push(span);
                }
                last_emitted_bytes = s.len();
            }

            byte_pos += c.len_utf8();
        }

        self.normalized = new_normalized;
        self.spans = new_spans;
        self
    }
}

impl From<&str> for NormalizedString {
    fn from(s: &str) -> Self {
        Self::from(s)
    }
}

impl From<String> for NormalizedString {
    fn from(s: String) -> Self {
        Self::from(s.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_round_trips() {
        let n = NormalizedString::from("hello");
        assert_eq!(n.get(), "hello");
        assert_eq!(n.get_original(), "hello");
        assert_eq!(n.len(), 5);
        assert_eq!(n.len_original(), 5);
        assert!(!n.is_empty());
    }

    #[test]
    fn empty_string_round_trip() {
        let n = NormalizedString::from("");
        assert!(n.is_empty());
        assert_eq!(n.get(), "");
        assert_eq!(n.get_original(), "");
        assert_eq!(n.len(), 0);
    }

    #[test]
    fn splice_range_empty_at_start() {
        let n = NormalizedString::from("hello");
        assert_eq!(n.splice_range_original(0..0), Some(0..0));
    }

    #[test]
    fn splice_range_empty_in_middle() {
        let n = NormalizedString::from("hello");
        assert_eq!(n.splice_range_original(3..3), Some(3..3));
    }

    #[test]
    fn splice_range_empty_at_end() {
        let n = NormalizedString::from("hello");
        assert_eq!(n.splice_range_original(5..5), Some(5..5));
    }

    #[test]
    fn splice_range_whole_string() {
        let n = NormalizedString::from("hello");
        assert_eq!(n.splice_range_original(0..5), Some(0..5));
    }

    #[test]
    fn splice_range_single_byte_at_start() {
        let n = NormalizedString::from("hello");
        assert_eq!(n.splice_range_original(0..1), Some(0..1));
    }

    #[test]
    fn splice_range_single_byte_at_end() {
        let n = NormalizedString::from("hello");
        assert_eq!(n.splice_range_original(4..5), Some(4..5));
    }

    #[test]
    fn splice_range_reverse_returns_none() {
        let n = NormalizedString::from("hello");
        // Explicit construction - `3..1` literal trips
        // `clippy::reversed_empty_ranges` because in idiomatic code such a
        // range is a programmer error. Here we're testing the function's
        // defensive guard against that exact mistake.
        let r = std::ops::Range::<usize> { start: 3, end: 1 };
        assert_eq!(n.splice_range_original(r), None);
    }

    #[test]
    fn splice_range_out_of_bounds_end_returns_none() {
        let n = NormalizedString::from("hello");
        assert_eq!(n.splice_range_original(0..6), None);
        assert_eq!(n.splice_range_original(0..1000), None);
    }

    #[test]
    fn splice_range_out_of_bounds_start_returns_none() {
        let n = NormalizedString::from("hello");
        assert_eq!(n.splice_range_original(10..20), None);
    }

    #[test]
    fn splice_range_after_pure_ascii_fold_is_identity() {
        // ascii_quotes on a string with no smart quotes is a no-op for the
        // chars but still allocates new buffers. Alignment must remain 1:1.
        let mut n = NormalizedString::from("Hello, World!");
        n.ascii_quotes();
        for start in 0..n.get().len() {
            for end in start..=n.get().len() {
                let splice = n.splice_range_original(start..end);
                assert_eq!(splice, Some(start..end), "wrong for {start}..{end}");
            }
        }
    }

    #[test]
    fn splice_range_after_collapse_at_string_start() {
        // Whitespace run at the very beginning. After collapse, the
        // surviving space's span end extends to cover the whole run.
        let mut n = NormalizedString::from("   hello");
        n.collapse_whitespace_runs();
        assert_eq!(n.get(), " hello");
        let splice = n.splice_range_original(0..1).unwrap();
        assert_eq!(splice, 0..3);
    }

    #[test]
    fn splice_range_after_collapse_at_string_end() {
        let mut n = NormalizedString::from("hello   ");
        n.collapse_whitespace_runs();
        assert_eq!(n.get(), "hello ");
        let splice = n.splice_range_original(5..6).unwrap();
        assert_eq!(splice, 5..8);
    }

    #[test]
    fn splice_range_after_collapse_covers_whole_normalised() {
        let mut n = NormalizedString::from("  a  b  ");
        n.collapse_whitespace_runs();
        let nlen = n.get().len();
        let olen = n.get_original().len();
        let splice = n.splice_range_original(0..nlen).unwrap();
        assert_eq!(splice, 0..olen);
    }

    #[test]
    fn splice_range_consistent_with_str_slicing() {
        // Property: the splice range maps a normalised match back to the
        // original bytes that the match "occupies", such that slicing the
        // original by that range gives the source the match came from.
        let original = "a    b\tc\nd";
        let mut n = NormalizedString::from(original);
        n.collapse_whitespace_runs();
        // Normalised "a b c d". Splice "b c" (norm bytes 2..5).
        let needle = "b c";
        let needle_pos = n.get().find(needle).unwrap();
        let splice = n
            .splice_range_original(needle_pos..needle_pos + needle.len())
            .unwrap();
        // Original "b\tc" is what was covered.
        assert_eq!(&original[splice], "b\tc");
    }

    #[test]
    fn splice_range_multibyte_char_unaffected() {
        // A multibyte char not touched by any transform. Each of its bytes
        // maps to the full char range.
        let n = NormalizedString::from("a\u{4E2D}b"); // CJK "中" = 3 bytes
        // For norm bytes 1..4 (the CJK), splice → original 1..4.
        let splice = n.splice_range_original(1..4).unwrap();
        assert_eq!(splice, 1..4);
    }

    #[test]
    fn splice_range_returns_char_boundary_in_original() {
        // Invariant: for an in-bounds range with both ends on normalised
        // char boundaries, the returned original range MUST also be on
        // char boundaries - otherwise `original[range]` panics.
        let original = "a\u{4E2D}b\u{4E2D}c";
        let mut n = NormalizedString::from(original);
        n.ascii_quotes(); // no-op on these chars but rebuilds spans
        for start in 0..=n.get().len() {
            if !n.get().is_char_boundary(start) {
                continue;
            }
            for end in start..=n.get().len() {
                if !n.get().is_char_boundary(end) {
                    continue;
                }
                let splice = n.splice_range_original(start..end).unwrap();
                assert!(
                    original.is_char_boundary(splice.start),
                    "splice.start {} not on char boundary for {start}..{end}",
                    splice.start
                );
                assert!(
                    original.is_char_boundary(splice.end),
                    "splice.end {} not on char boundary for {start}..{end}",
                    splice.end
                );
            }
        }
    }

    #[test]
    fn nfkc_recomposes_decomposed() {
        // "e\u{0301}" (NFD) → "é" (NFC/NFKC composed).
        let mut n = NormalizedString::from("e\u{0301}gant");
        n.nfkc();
        assert_eq!(n.get(), "égant");
        assert_eq!(n.get_original(), "e\u{0301}gant");
        // For splicing: a match covering "é" (norm bytes 0..2) covers the
        // original 'e' + combining accent (bytes 0..3) - the absorbed
        // combining accent is included so the splice doesn't leave it
        // behind.
        let splice = n.splice_range_original(0..2).unwrap();
        assert_eq!(splice, 0..3);
    }

    #[test]
    fn nfkc_normalises_fullwidth_letters() {
        // NFKC maps fullwidth ASCII (U+FF21..U+FF3A) to ASCII halves.
        // "Ａ" (U+FF21) is 3 bytes; "A" is 1 byte.
        let mut n = NormalizedString::from("\u{FF21}BC");
        n.nfkc();
        assert_eq!(n.get(), "ABC");
        // Splicing the normalised "A" should cover the original 3 bytes.
        let splice = n.splice_range_original(0..1).unwrap();
        assert_eq!(splice, 0..3);
    }

    #[test]
    fn nfkc_pure_ascii_is_identity() {
        let mut n = NormalizedString::from("hello world");
        n.nfkc();
        assert_eq!(n.get(), "hello world");
        // No alignment change.
        let splice = n.splice_range_original(0..n.get().len()).unwrap();
        assert_eq!(splice, 0..n.get_original().len());
    }

    #[test]
    fn ascii_quotes_3byte_to_1byte_preserves_alignment() {
        // Smart quote is 3 bytes; ASCII '"' is 1 byte. The new 1-byte
        // span must still point at the full original 3-byte range.
        let mut n = NormalizedString::from("\u{201C}x\u{201D}");
        n.ascii_quotes();
        assert_eq!(n.get(), "\"x\"");
        // Splice the opening quote (norm byte 0..1) → original bytes 0..3.
        let splice = n.splice_range_original(0..1).unwrap();
        assert_eq!(splice, 0..3);
        // Splice 'x' (norm byte 1..2) → original 3..4.
        let splice = n.splice_range_original(1..2).unwrap();
        assert_eq!(splice, 3..4);
        // Splice closing quote → original 4..7.
        let splice = n.splice_range_original(2..3).unwrap();
        assert_eq!(splice, 4..7);
    }

    #[test]
    fn chain_nfkc_then_ascii_quotes() {
        // "ﬀ" (U+FB00 ff ligature, 3 bytes) → NFKC → "ff" (2 bytes).
        let mut n = NormalizedString::from("\u{FB00}");
        n.nfkc().ascii_quotes();
        assert_eq!(n.get(), "ff");
        // Splice the first 'f' should cover the whole ligature
        // (ligature is consumed as one source char with inserted second 'f').
        let splice = n.splice_range_original(0..1).unwrap();
        assert_eq!(splice, 0..3);
        // Splice the second 'f' (the inserted one) should also cover the
        // original ligature region - it shares the span with the first.
        let splice = n.splice_range_original(0..2).unwrap();
        assert_eq!(splice, 0..3);
    }

    #[test]
    fn ascii_quotes_folds_smart_singles() {
        let mut n = NormalizedString::from("a\u{2018}b\u{2019}c");
        n.ascii_quotes();
        assert_eq!(n.get(), "a'b'c");
        // Each output char's alignment still maps back to its original source byte.
        // '\u{2018}' takes 3 bytes in the original; the ASCII replacement's
        // span still covers the full 3-byte original range.
        assert_eq!(n.get_original(), "a\u{2018}b\u{2019}c");
        let splice = n.splice_range_original(1..2).unwrap();
        assert_eq!(splice, 1..4);
    }

    #[test]
    fn ascii_quotes_folds_smart_doubles() {
        let mut n = NormalizedString::from("\u{201C}hi\u{201D}");
        n.ascii_quotes();
        assert_eq!(n.get(), "\"hi\"");
    }

    #[test]
    fn ascii_dashes_folds_em_and_en() {
        let mut n = NormalizedString::from("a\u{2013}b\u{2014}c\u{2212}d");
        n.ascii_dashes();
        assert_eq!(n.get(), "a-b-c-d");
    }

    #[test]
    fn ascii_spaces_folds_nbsp() {
        let mut n = NormalizedString::from("a\u{00A0}b\u{3000}c");
        n.ascii_spaces();
        assert_eq!(n.get(), "a b c");
    }

    #[test]
    fn collapse_whitespace_simple_run() {
        let mut n = NormalizedString::from("a    b");
        n.collapse_whitespace_runs();
        assert_eq!(n.get(), "a b");
        assert_eq!(n.get_original(), "a    b");
        // splice_range_original on the collapsed space MUST cover the
        // entire absorbed run - otherwise an Edit splice would leave the
        // extra whitespace in the file.
        let splice = n.splice_range_original(1..2).unwrap();
        assert_eq!(splice, 1..5);
    }

    #[test]
    fn collapse_whitespace_mixed() {
        let mut n = NormalizedString::from("a\t\tb\n\nc");
        n.collapse_whitespace_runs();
        assert_eq!(n.get(), "a b c");
    }

    #[test]
    fn collapse_whitespace_leading_and_trailing() {
        let mut n = NormalizedString::from("   a   ");
        n.collapse_whitespace_runs();
        assert_eq!(n.get(), " a ");
    }

    #[test]
    fn collapse_whitespace_only_whitespace() {
        let mut n = NormalizedString::from("   ");
        n.collapse_whitespace_runs();
        assert_eq!(n.get(), " ");
    }

    #[test]
    fn collapse_whitespace_no_runs_is_identity() {
        let mut n = NormalizedString::from("hello");
        n.collapse_whitespace_runs();
        assert_eq!(n.get(), "hello");
    }

    #[test]
    fn ascii_quotes_each_single_quote_codepoint() {
        for &c in &['\u{2018}', '\u{2019}', '\u{201A}', '\u{201B}'] {
            let s = format!("a{c}b");
            let mut n = NormalizedString::from(s.as_str());
            n.ascii_quotes();
            assert_eq!(n.get(), "a'b", "failed for U+{:04X}", c as u32);
        }
    }

    #[test]
    fn ascii_quotes_each_double_quote_codepoint() {
        for &c in &['\u{201C}', '\u{201D}', '\u{201E}', '\u{201F}'] {
            let s = format!("a{c}b");
            let mut n = NormalizedString::from(s.as_str());
            n.ascii_quotes();
            assert_eq!(n.get(), "a\"b", "failed for U+{:04X}", c as u32);
        }
    }

    #[test]
    fn ascii_dashes_each_codepoint() {
        for &c in &[
            '\u{2010}', '\u{2011}', '\u{2012}', '\u{2013}', '\u{2014}', '\u{2015}', '\u{2212}',
        ] {
            let s = format!("a{c}b");
            let mut n = NormalizedString::from(s.as_str());
            n.ascii_dashes();
            assert_eq!(n.get(), "a-b", "failed for U+{:04X}", c as u32);
        }
    }

    #[test]
    fn ascii_spaces_each_codepoint() {
        let codepoints = [
            '\u{00A0}', '\u{2002}', '\u{2003}', '\u{2004}', '\u{2005}', '\u{2006}', '\u{2007}',
            '\u{2008}', '\u{2009}', '\u{200A}', '\u{202F}', '\u{205F}', '\u{3000}',
        ];
        for &c in &codepoints {
            let s = format!("a{c}b");
            let mut n = NormalizedString::from(s.as_str());
            n.ascii_spaces();
            assert_eq!(n.get(), "a b", "failed for U+{:04X}", c as u32);
        }
    }

    #[test]
    fn ascii_quotes_no_smart_quotes_is_identity() {
        let original = "let s = \"hello\";";
        let mut n = NormalizedString::from(original);
        n.ascii_quotes();
        assert_eq!(n.get(), original);
        let splice = n.splice_range_original(0..n.get().len()).unwrap();
        assert_eq!(splice, 0..original.len());
    }

    #[test]
    fn ascii_dashes_no_unicode_dashes_is_identity() {
        let original = "a-b-c";
        let mut n = NormalizedString::from(original);
        n.ascii_dashes();
        assert_eq!(n.get(), original);
    }

    #[test]
    fn ascii_spaces_ascii_space_is_identity() {
        let original = "a b c";
        let mut n = NormalizedString::from(original);
        n.ascii_spaces();
        assert_eq!(n.get(), original);
    }

    #[test]
    fn collapse_whitespace_handles_form_feed_and_vertical_tab() {
        // \x0B (vertical tab) and \x0C (form feed) are considered whitespace
        // by char::is_whitespace.
        let mut n = NormalizedString::from("a\x0B\x0C\tb");
        n.collapse_whitespace_runs();
        assert_eq!(n.get(), "a b");
    }

    #[test]
    fn collapse_whitespace_carriage_return_collapses_with_lf() {
        let mut n = NormalizedString::from("a\r\nb");
        n.collapse_whitespace_runs();
        assert_eq!(n.get(), "a b");
        let splice = n.splice_range_original(1..2).unwrap();
        assert_eq!(splice, 1..3);
    }

    #[test]
    fn collapse_whitespace_lone_cr_is_whitespace() {
        let mut n = NormalizedString::from("a\rb");
        n.collapse_whitespace_runs();
        assert_eq!(n.get(), "a b");
    }

    #[test]
    fn collapse_whitespace_single_space_is_identity() {
        let original = "a b c";
        let mut n = NormalizedString::from(original);
        n.collapse_whitespace_runs();
        assert_eq!(n.get(), "a b c");
        for i in 0..n.get().len() {
            let splice = n.splice_range_original(i..i + 1).unwrap();
            assert_eq!(splice, i..i + 1);
        }
    }

    #[test]
    fn collapse_whitespace_alignment_preserves_neighbours() {
        // After collapse, splicing a neighbouring non-whitespace char should
        // NOT extend into the absorbed whitespace run.
        let mut n = NormalizedString::from("a   b");
        n.collapse_whitespace_runs();
        assert_eq!(n.get(), "a b");
        let splice = n.splice_range_original(0..1).unwrap();
        assert_eq!(splice, 0..1);
        let splice = n.splice_range_original(2..3).unwrap();
        assert_eq!(splice, 4..5);
    }

    #[test]
    fn each_extension_on_empty_string() {
        for _f in 0..4 {
            let mut n = NormalizedString::from("");
            n.ascii_quotes()
                .ascii_dashes()
                .ascii_spaces()
                .collapse_whitespace_runs();
            assert_eq!(n.get(), "");
        }
    }

    #[test]
    fn each_extension_on_single_char() {
        let mut n = NormalizedString::from("\u{2014}");
        n.ascii_dashes();
        assert_eq!(n.get(), "-");
        assert_eq!(n.get_original(), "\u{2014}");
    }

    #[test]
    fn chain_all_four_preserves_original_alignment() {
        let input = "let s = \u{201C}foo \u{2014} bar\u{201D};\n\tlet t =\u{00A0}\u{00A0}1;";
        let mut n = NormalizedString::from(input);
        n.nfkc()
            .ascii_quotes()
            .ascii_dashes()
            .ascii_spaces()
            .collapse_whitespace_runs();
        assert!(n.get().contains("\"foo - bar\""));
        assert_eq!(n.get_original(), input);
        let needle_pos = n.get().find("foo - bar").unwrap();
        let needle_end = needle_pos + "foo - bar".len();
        let orig_range = n.splice_range_original(needle_pos..needle_end).unwrap();
        let orig_slice = &input[orig_range.clone()];
        assert!(orig_slice.contains("foo"));
        assert!(orig_slice.contains("bar"));
        assert!(orig_slice.contains('\u{2014}'));
    }
}
