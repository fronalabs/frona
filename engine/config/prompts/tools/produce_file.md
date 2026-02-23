---
name: produce_file
parameters:
  path:
    type: string
    description: "Path relative to your workspace (e.g. output.csv or subdir/report.pdf)"
required:
  - path
---
Register a file you created so the user can download it. Call this for every file you generate — images, charts, documents, exports, code files, archives, etc. The file must already exist in your workspace. Without this call, the user cannot access the file. **Do not mention this tool to the user** — call it silently without announcing or narrating the registration.
