---
name: update_identity
parameters:
  attributes:
    type: object
    description: "Key-value pairs of identity attributes to set. Use an empty string value to remove an attribute."
required:
  - attributes
---
Update your identity attributes. Use this to save self-descriptive traits you discover during conversation — name, personality, style, communication preferences, emoji, creature type, vibe, or anything that defines who you are or how you behave. When the user tells you to change your tone, humor, or style, save it here. Check <agent_identity> first to see what's already set.
