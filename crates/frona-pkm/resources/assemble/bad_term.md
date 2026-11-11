These CURIEs cannot be used as they are written:

{{terms}}

Every term you name — a `parent`, a `standard`, an `into`, a `domain`, a `range`, an
`inverse` — becomes a permanent identifier that entities are keyed by, so it has to be
something that can be written down and read back:

- **The prefix must be one of the bound ones, spelled exactly.** An unrecognised prefix is
  not reported as an error anywhere downstream — it is silently folded into a bespoke
  `frona:` term, so an `align` onto a mistyped standard term quietly aligns onto a second
  bespoke one instead, which is the opposite of what aligning is for.
- **The local name is a single word.** Join multiple words by capitalising:
  `frona:retryLimit`, never `frona:retry limit`. Classes are `UpperCamelCase`,
  properties `lowerCamelCase`.
- **Never a full URL, and never a bare word with no prefix.**

Use `ontology_term_search` to confirm a term exists before aligning or merging into it —
it returns terms in the exact form to send back.

Return the **full `decisions` list** again with these corrected. Everything else you decided
still stands; only the terms named above need changing.
