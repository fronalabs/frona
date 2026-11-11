These CURIEs cannot be used as they are written:

{{terms}}

A term becomes a permanent identifier that entities are keyed by, so it has to be something
that can be written down and read back:

- **The prefix must be one of the bound ones, spelled exactly.** An unrecognised prefix is
  not reported as an error anywhere downstream — it is silently folded into a bespoke
  `frona:` term, so a typo becomes a new permanent concept instead of the standard one you
  meant.
- **The local name is a single word.** Join multiple words by capitalising:
  `frona:retryLimit`, never `frona:retry limit`. Classes are `UpperCamelCase`,
  relations and attributes `lowerCamelCase`.
- **Never a full URL, and never a bare word with no prefix.** If the term you want does not
  exist under a bound prefix, mint it under `frona:` rather than reaching outside them.

Use `ontology_term_search` if you are unsure whether a term exists — it returns terms in
the exact form to send back.

Nothing from the rejected submission was kept, so **re-send all fields** — `entity`,
`classes`, `relations`, `attributes`, `new_entities`, `declarations`, `has_keys`, and
`inverse_functional_properties` — with these terms corrected and everything else exactly
as you had it.
