---
name: make_voice_call
parameters:
  phone_number:
    type: string
    description: Phone number to call in E.164 format (e.g. +15555551234)
  name:
    type: string
    description: Name of the person, company, or whoever is being called (used to identify or create a contact record)
  objective:
    type: string
    description: The specific goal of this call (e.g. "make a dinner reservation for 2 tonight at 7pm")
  initial_greeting:
    type: string
    description: Optional message spoken by the agent immediately when the call connects, before the caller speaks.
  hints:
    type: string
    description: Optional comma-separated words or phrases to improve speech recognition accuracy (e.g. "confirm, cancel, repeat").
required:
  - phone_number
  - name
  - objective
---
Place an outbound voice call on behalf of the user.

TAG REFERENCE
[CALL_CONNECTED] appears in the tool result when make_voice_call executes.
  Format: [CALL_CONNECTED: Now speaking with <name> (<number>). Goal: <objective>.]
[LIVE_CALL] prefixes every subsequent message from the called party (transcribed speech).
  Format: [LIVE_CALL] <what they said>

When you see [CALL_CONNECTED] in your tool result, switch immediately to Outbound Agent mode:

CALL TRANSITION RULES

- Every [LIVE_CALL] message is what the called party just said. Reply only to them, not to the user.
- Use plain spoken English only. No markdown, no bullet points, no asterisks, no bold.
- Be brief. Do not small-talk unless they initiate.
- Speak numbers digit-by-digit (e.g. "six, five, zero" not "six hundred fifty").
- Execute the Goal from [CALL_CONNECTED]. Stay on task.
- When the task is complete, call hangup_call.

Example:
Tool result: [CALL_CONNECTED: Now speaking with Zoka Restaurant (111-111-1111). Goal: dinner reservation for 2 tonight.]
[LIVE_CALL] Hi, this is Zoka Restaurant, how can I help?
Response: Hi, I'm calling to make a dinner reservation for 2 people tonight. Is that possible?

After placing the call, send one short confirmation to the user (e.g. "Call placed."). Nothing more.
