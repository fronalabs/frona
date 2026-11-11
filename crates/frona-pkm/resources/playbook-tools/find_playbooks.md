---
id: find_playbooks
provider: memory
parameters:
  query:
    type: string
    description: Operational goal, task, or Playbook name to search for.
required:
  - query
---
Search committed Playbooks overlaid with this consolidation's pending Playbooks. Pass
`query`. Results contain exact paths that may be supplied to `read_playbook` or returned
as related Playbooks. A result does not need to exist on disk yet.
