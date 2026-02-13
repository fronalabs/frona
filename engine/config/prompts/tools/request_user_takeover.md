---
name: request_user_takeover
parameters:
  reason:
    type: string
    description: Why user intervention is needed
required:
  - reason
---
Request the user to take over the browser session (e.g. for CAPTCHA, 2FA, login). The debugger URL is automatically generated from the last browser profile used. Creates a notification and returns immediately.
