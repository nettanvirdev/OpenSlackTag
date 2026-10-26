---
name: Incident summary
description: Turn an incident thread into a blameless summary with timeline, cause and follow-ups
---

# Incident summary

Use this when asked to summarise an incident, outage or postmortem thread.

1. Read the whole thread with `slack_read_thread` (and any linked threads).
2. Build a timeline in the server's time zone: detection, escalation, mitigation,
   resolution. Use the messages' times; never invent times.
3. Write it blameless: describe what systems and processes did, not who erred.
4. Structure:
   - *Summary*: two sentences, impact first (who was affected, for how long).
   - *Timeline*: bullet per event.
   - *Cause*: what is known, and what is still a hypothesis (say which).
   - *Follow-ups*: each with an owner (`<@U…>`) if one was named, else "owner needed".
5. Save it to the wiki as `incidents/<yyyy-mm-dd>-<short-name>` and link the page in your reply.
