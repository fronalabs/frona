---
id: create_agent
provider: agent
parameters:
  id:
    type: string
    description: "A slug-like identifier for the agent (lowercase, hyphens only, e.g. 'research-assistant'). Used in workspace paths and routing."
  name:
    type: string
    description: "The display name for the agent (e.g. 'Research Assistant')"
  summary:
    type: string
    description: "A brief one-line summary of what this agent does"
  instructions:
    type: string
    description: "The full system prompt that defines the agent's behavior, personality, constraints, and capabilities. This is the agent's entire guide — be thorough."
  model_group:
    type: string
    description: "Exact configured model group name, for example 'primary', 'coding', or 'reasoning'. Required; provider/model shorthand is not supported."
  tools:
    type: array
    items:
      type: string
    description: "List of tool names to enable. If omitted, all standard tools are enabled."
required:
  - id
  - name
  - summary
  - instructions
  - model_group
---
Create a new agent with the given configuration. The agent will be immediately available for use.

## Before calling this tool

Interview the user to understand what they need. Don't dump a form — have a natural conversation, asking one or two questions at a time.

Gather:
1. **Purpose** — What should this agent do? What's its scope and specialty?
2. **Behavior** — How should it communicate? Tone, style, constraints, things to avoid?
3. **Tools** — Which capabilities does it need?

## Writing instructions

The `instructions` field becomes the agent's entire personality and behavior guide. The agent will **only** know what you write here. Be specific and thorough — include:
- What the agent does and doesn't do
- Communication style and tone
- Step-by-step workflows if applicable
- Constraints and boundaries
- How to handle edge cases

## Choosing an ID

Pick a short, descriptive slug: `research-assistant`, `code-reviewer`, `data-analyst`. Lowercase letters and hyphens only.

## Confirmation

Before calling this tool, summarize what you're about to create and confirm with the user.
