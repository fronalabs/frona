Some submitted fields did not resolve against their declared source messages:

{{rejected}}

Accepted memories and their stable evidence references:

{{accepted_memories}}

Memories that need correction or removal, with their current evidence references:

{{memory_repairs}}

The accepted memories are not part of the current repair. Keep their IDs and evidence
references unchanged.
You can include them unchanged in the full result, or omit them from the correction;
omitting an accepted ID does not delete it. The server retains it. Do not use a new ID for
an accepted memory.

Each failure names the exact output field, declared message, submitted text or value, and
truthful reason. It also lists the only `allowed changes` for this correction. The server
retains accepted state and applies only those fields from the resubmission; unrelated
changes are ignored. When two semantic fields must change together—such as `kind` and
`episode`—both are listed.

Keep every memory, new-entity, and candidate-attribute `id` unchanged while correcting it. Array
order and a proposed entity path are not identity. You may correct an entity path and its semantic
fields while keeping its ID. A split-off contribution gets a new ID; a removed contribution is
omitted by ID.

Correct citations and values against their declared messages. Do not move a quote to an
unrelated message merely because similar text appears elsewhere. Replacement citations
must jointly support the complete unchanged claim, not merely mention one entity from it.
Use multiple exact source spans when separate spans support separate atomic parts.
Write every critical number, date, URL, and identifier in the same literal form as the
selected evidence. Do not convert `20k` to `20,000` or make a similar semantic rewrite.
Validation ignores case, spaces, and punctuation only.

For an Agent-sourced claim, call `search_tool_evidence` for its declared Agent message.
Keep the Agent's exact transcript citation in `sources`, then add a separate
`tool_evidence` selection with the returned evidence ID and a single contiguous exact span from that
result's sanitized `request` or response `text`. Copy it verbatim. Do not use ellipses, join separate
passages, summarize, or normalize the returned text.
Otherwise reformulate the claim to exactly what the evidence supports, or drop that claim
and its unsupported entity/attribute/playbook contribution.
`memory_search`, Memory-vault reads, and memory mutations are never acceptable support.
An Agent-sourced claim requires a successful non-recall tool execution from its evidence
horizon unless a later User explicitly confirms it or a structured task-lifecycle source
supports it. A matching recall does not veto genuinely independent execution evidence.

The two evidence channels must remain separate:

```json
"sources": [{"message":"m2","quote":"The deployment succeeded.","strength":"derived","confirmation":false}],
"tool_evidence": [{"message":"m2","evidence_id":"e3:chunk1","quote":"status=green failed_checks=0"}]
```

Evidence IDs are extraction-conversation-local and identify one execution-result chunk.
The same execution chunk keeps the same ID when found from another Agent message.
Every `tool_evidence.message` must match an Agent citation in the same contribution's
`sources`. Do not invent an ID. The server already knows the selected
tool kind, call ID, query, URL, request, and result. Tool requests are sanitized; secrets
are never available.

If `research_message_unaccounted` is listed, inspect that message's tool evidence. Add its
grounded memories or return one explicit `no_durable_claim`, `duplicate`, or `unsupported`
disposition with a reason. This coverage repair may append memories. It must preserve all
accepted memories. If one mixed claim contains supported and unsupported facts, split it
and keep every independently supported fact. When `tool_evidence_clause_mismatch` names an
unsupported clause, keep the supported clause under the original contribution ID. Remove
or correct only the named clause. Give each additional split contribution a new ID.

Return the full result again and correct the listed fields. Nothing has been discarded yet;
terminal cleanup happens only after the correction budget is exhausted. Accepted fields
come from server-retained state, so changing or omitting them does not alter the result.
