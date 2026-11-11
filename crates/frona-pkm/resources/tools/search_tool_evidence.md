---
id: search_tool_evidence
provider: memory
parameters:
  message_id:
    type: string
    description: The prompt-local Agent message ID being grounded, such as m2.
  query:
    type: string
    description: A concise factual claim or phrase to match against qualified executions in this extraction window.
required:
  - message_id
  - query
---
Search the bounded, sanitized requests and results of successful non-recall tool executions
available to the named Agent message's evidence horizon. The response directly returns ranked,
citable result chunks with server-controlled evidence IDs such as `m2:tool1`.
It does not expose secrets or rerun tools.

Search before submitting any memory or candidate attribute sourced from an Agent message.
After finding support, keep the exact Agent assertion in the contribution's `sources` and
add a separate `tool_evidence` entry containing the returned `message`, `evidence_id`, and
smallest exact quote from its sanitized `request` or response `text`. The request can prove
what action was attempted and the response can prove its outcome; select multiple returned
IDs when separate executions jointly support one atomic claim. The evidence message must
match the Agent citation in that contribution's `sources`.

Do not submit execution IDs, tool kinds, queries, URLs, or other internal provenance
metadata; the server resolves them from the ID. Do not treat ranking as proof: reformulate
the claim to match the evidence or drop it when the execution does not actually support it.
