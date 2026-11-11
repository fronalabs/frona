Some of your decisions were **not applied**. The system dry-ran each one against the
real knowledge base and these failed its guardrails:

{{rejections}}

Revise only the listed terms — everything else was accepted and will be committed.

- A **contradiction** means the schema itself becomes unsatisfiable. The term cannot be
  declared that way at all: pick a different parent, a different alignment target, or
  `defer` it.
- **Would break N existing facts** means the edit is logically sound but too many entities
  already contradict it. Loosen it (a more general parent, a wider range, looser
  bounds), pick a different target, or `defer` it. Do not simply resubmit the same
  decision.

Return the full `decisions` list again, with the rejected terms revised.
