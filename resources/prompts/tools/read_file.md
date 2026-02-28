---
name: read_file
parameters:
  path:
    type: string
    description: "Virtual file path (e.g. user://uid/report.pdf or agent://dev/output.csv)"
  offset:
    type: integer
    description: "Line offset to start reading from (text files only, default 0)"
    default: 0
  limit:
    type: integer
    description: "Maximum number of lines to read (text files only, default 500)"
    default: 500
required:
  - path
---
Read a file from the virtual filesystem. Accepts paths like user://user-id/filename or agent://agent-id/path. For text files, returns the content with optional offset and limit. For images, returns the image for visual analysis. For binary files, returns file metadata.
