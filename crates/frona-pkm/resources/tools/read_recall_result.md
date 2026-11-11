---
id: read_recall_result
provider: memory
parameters:
  call_id:
    type: string
    description: A prompt-local recall call ID such as T1, shown beside an Agent message.
required:
  - call_id
---
Read the bounded stored result of a prior memory_search or knowledge-page read associated
with this extraction window. This never reruns the original retrieval. The initial transcript
shows the recall operation and query or path without its result; read the result only when
that metadata is insufficient to decide whether an Agent claim is recalled or genuinely new.
