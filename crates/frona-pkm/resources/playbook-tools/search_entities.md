---
id: search_entities
provider: memory
parameters:
  query:
    type: string
    description: A name or phrase to search for in the effective knowledge state.
required:
  - query
---
Search production entities overlaid with the current consolidation's pending entities. Returns
exact entity paths, names, descriptions, and types. Results may not exist on disk yet. Pass
any returned path to `read_entity` for its full effective state.
