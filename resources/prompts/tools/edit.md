---
id: edit
provider: file
parameters:
  path:
    type: string
    description: "Path to the file to edit. Bare paths resolve to your workspace."
  old_string:
    type: string
    description: "Text to find. Exact matches take priority; if none exist, matching tolerates whitespace, smart quotes, Unicode dashes, and line endings. The selected match must be unique unless replace_all is true."
  new_string:
    type: string
    description: "Replacement text. Substituted in place of old_string while preserving the file's original line endings."
  replace_all:
    type: boolean
    description: "Replace all exact and normalized occurrences of old_string instead of failing on multiple matches. Use for rename-everywhere refactors."
required:
  - path
  - old_string
  - new_string
---
Make a surgical edit to an existing file. A single edit prefers **exact matches**: one exact match wins over normalized near-matches; multiple exact matches require more context. If there are no exact matches, matching tolerates whitespace/punctuation (smart quotes, Unicode dashes, special spaces, CRLF/LF, trailing whitespace), but the normalized match must be unique. With `replace_all: true`, both exact and normalized occurrences are replaced, including mixed spellings. Boundary whitespace is limited to what `old_string` represents so neighboring lines and indentation remain intact. If `old_string` matches zero locations the file may have drifted; re-read it. Prefer this over `sed` for any change to a file you've already created.
