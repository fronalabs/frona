---
id: memory_search
provider: memory
parameters:
  query:
    type: string
    description: What you're looking for. Use the user's terms — names like 'home assistant', 'frona', or short descriptive phrases.
required:
  - query
---
Search your knowledge base. Returns up to 8 ranked pages (people, projects, services, places, topics, and playbooks), each with its name, a one-line description, a query-relevant body excerpt when available, a type tag, and an **absolute file path**. `read(<path>)` to open one — pages are self-describing markdown: prose body plus YAML frontmatter carrying the structured facts (`attributes:`) and links. A `[playbook]` tag marks a reusable how-to procedure (for "how do I X?"); other tags are the concept kind (service, person, project…).
