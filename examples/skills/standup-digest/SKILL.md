---
name: Standup digest
description: Summarise async standup updates in a channel into one digest, with blockers first
---

# Standup digest

Use this for "summarise today's standup" or a morning standup routine.

1. Read the standup channel for the last 24 hours (`slack_read_channel`, `hours: 24`),
   including threads with replies (`slack_read_thread`).
2. Put **blockers** first, each with who is blocked and on what, and anyone mentioned
   who could unblock them.
3. Then one line per person: what they are on today.
4. List people who usually post but didn't today (from the wiki page `team/people`,
   if it exists) under *No update yet*. Do not guess who "usually" posts without that page.
5. Keep it short enough to read in 30 seconds.
