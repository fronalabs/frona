---
id: memory_graph_sparql
provider: memory
parameters:
  query:
    type: string
    description: A SPARQL 1.1 SELECT or ASK query. Built-in prefixes include schema:, foaf:, skos:, dcterms:, frona:, rdf:, rdfs:, owl:, and xsd:.
required:
  - query
---
Run a read-only SPARQL SELECT or ASK query against the user's reasoned memory graph. The
graph contains the effective ontology and knowledge entities in one materialized closure.
Knowledge entity IRIs have the form <urn:frona:kb:{page-path}>. Named results are returned
as compact CURIEs or page paths. Every concept entity has a `schema:name` literal and a
`schema:identifier` literal containing its page path without the `.md` extension. SELECT
results are capped at 200 rows. Use memory_search to resolve entity paths or ontology
classes and memory_graph_get to inspect one entity.
