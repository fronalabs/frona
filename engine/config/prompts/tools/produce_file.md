---
name: produce_file
parameters:
  path:
    type: string
    description: "Path relative to your workspace (e.g. output.csv or subdir/report.pdf)"
required:
  - path
---
Register a file you created as a produced output so the user can download it. **You must call this for every file you generate for the user** — images, charts, documents, exports, code files, archives, etc. The file must already exist in your workspace. Without this call, the user cannot access the file.
