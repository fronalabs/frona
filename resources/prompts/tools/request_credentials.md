---
name: request_credentials
parameters:
  query:
    type: string
    description: Search term to find the vault item (e.g. "home assistant", "github", "aws")
  reason:
    type: string
    description: Why you need this credential (shown to the user in the approval prompt)
  env_var_prefix:
    type: string
    description: If provided, each field of the credential is injected as a separate environment variable with this prefix (e.g. prefix "GITHUB" creates GITHUB_USERNAME, GITHUB_PASSWORD). The actual secret values are never shown to you — only the env var names. If omitted, the secret is returned directly.
  force:
    type: boolean
    description: If true, bypasses any existing grant and triggers the approval flow again. Use when previously fetched credentials didn't work (e.g. login failed, API returned 401).
required:
  - query
  - reason
---
Request credentials from the user's vault (password manager). The user will be prompted to approve and select the specific vault item. If a previous grant exists for this query, the credentials are returned immediately without prompting.

When env_var_prefix is set, credentials are injected as environment variables for subsequent CLI tool calls — you never see the actual values. Use this mode whenever possible to keep secrets secure.

Once credentials are loaded in a chat, they persist as environment variables for the rest of that chat session — you do not need to request them again within the same conversation.
