---
name: produce_file
parameters:
  path:
    type: string
    description: "Path relative to your workspace (e.g. output.csv or subdir/report.pdf)"
required:
  - path
---
Register a file from your workspace as a produced output. The file must already exist in your workspace. This makes the file available for the user to download and propagates it to parent agents when completing delegated tasks.
