---
id: memory_graph_get
provider: memory
parameters:
  path:
    type: string
    description: The knowledge entity path, such as people/sarah or services/postgres.
  direction:
    type: string
    enum: [outgoing, incoming, both]
    description: Which relation directions to return. Defaults to both.
  relation:
    type: string
    description: Optional relation CURIE used to filter neighbors, such as schema:worksFor. Omit this field when unused.
  limit:
    type: integer
    minimum: 1
    maximum: 100
    description: Maximum incoming and outgoing edges to return. Defaults to 50.
required:
  - path
---
Read one entity from the user's reasoned memory graph. Returns all direct and inferred
types, literal attributes, and selected incoming or outgoing entity relations. The path
is the knowledge page path, without the .md extension. Results reflect the last completed
memory consolidation.
