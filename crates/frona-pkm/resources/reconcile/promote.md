Some attribute values may refer to entities that already exist:

{{suggestions}}

These are suggestions, not required corrections. Review all of them together.

If an attribute value denotes one of the offered entities, remove that value from
`attributes` and add an entry to `entity_relations`:

{
  "attribute": "<the original attribute key>",
  "value": "<the exact original value>",
  "property": "<object-property CURIE, such as schema:memberOf or frona:usesTool>",
  "target": "<one offered entity path>"
}

If the value is genuinely a literal, leave it unchanged. It is valid to accept none of
these suggestions. Choose a relation-specific object property; do not simply reuse a
known data-property attribute key. Prefer an existing standard property when you know the
correct one, otherwise use a precise new `frona:` property. Re-send the complete verdict,
preserving every unrelated decision.
