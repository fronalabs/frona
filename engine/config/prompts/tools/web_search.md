---
name: web_search
parameters:
  query:
    type: string
    description: The search query
  max_results:
    type: integer
    description: "Maximum number of results to return (default: 5, max: 20)"
required:
  - query
---
Search the web and return structured results with titles, URLs, and snippets.
