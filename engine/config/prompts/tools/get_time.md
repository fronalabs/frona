---
name: get_time
parameters:
  add_minutes:
    type: integer
    description: Minutes to add (negative to subtract)
  add_hours:
    type: integer
    description: Hours to add (negative to subtract)
  add_days:
    type: integer
    description: Days to add (negative to subtract)
  add_weeks:
    type: integer
    description: Weeks to add (negative to subtract)
  add_months:
    type: integer
    description: Months to add (negative to subtract)
---
Get the current UTC time, or compute a future/past time by adding offsets. Call with no arguments to get the current time.
