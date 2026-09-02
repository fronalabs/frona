---
id: memory_search
provider: memory
parameters:
  query:
    type: string
    description: A complete entity name, ontology class, or short phrase describing the memory you need.
required:
  - query
---
Search memory using exact entity identity, effective-ontology class membership, page
metadata, and page body text. Returns one JSON result per page path, ranked by the
strongest evidence. `matched_by` explains each match. A `body_text` match includes a
verbatim page snippet. Use a short keyword bundle when the exact name is unknown; the
server combines exact path and class-token evidence without separate filters. Use the
absolute `file` path with `read` for prose and evidence, or pass `path` to
memory_graph_get or memory_graph_sparql for structured graph work.
