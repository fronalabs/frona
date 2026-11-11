---
id: read_playbook
provider: memory
parameters:
  path:
    type: string
    description: Exact Playbook path supplied by the phase or returned by find_playbooks.
required:
  - path
---
Read one Playbook from the effective knowledge state. Pass an exact path supplied by the
phase or returned by `find_playbooks`. The Playbook does not need to be authored yet.
