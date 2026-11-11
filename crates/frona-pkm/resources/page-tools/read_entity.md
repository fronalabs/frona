---
id: read_entity
provider: memory
parameters:
  path:
    type: string
    description: Exact entity path supplied by the phase or returned by search_entities.
required:
  - path
---
Read one entity from the effective knowledge state: the production entity overlaid with this
consolidation's pending changes. Returns its identity, schema types, attributes, assertions,
grounded identity evidence, source memories, and authored body when one exists. This is a
database read; the entity does not need to exist on disk.
