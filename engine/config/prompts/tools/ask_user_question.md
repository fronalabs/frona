---
name: ask_user_question
parameters:
  question:
    type: string
    description: The question to ask
  options:
    type: array
    items:
      type: string
    description: Available answer options
required:
  - question
  - options
---
Ask the user a question and wait for their response. Creates a notification and returns immediately.
