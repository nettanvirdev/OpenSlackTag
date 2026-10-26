---
name: Weekly report
description: How to write the team's Friday weekly report from Slack, the wiki and GitHub
---

# Weekly report

Use this when someone asks for the weekly report, or when a routine runs it.

1. Read the week's activity:
   - `slack_read_channel` for each channel listed on the wiki page `processes/weekly-report`
     (default: this channel), with `hours: 168`.
   - If a GitHub MCP server is connected, list merged pull requests from the last 7 days.
2. Group what happened into **Shipped**, **In progress**, **Blocked** and **Decisions**.
   One line per item, with who did it (`<@U…>`) and a link.
3. Keep it under 25 lines. Lead with the one thing a busy reader must know.
4. If the wiki page `processes/weekly-report` doesn't exist yet, create it with the channels
   you used, so next week's report is consistent.

Format:

```
*Week of <date>*
*Shipped*
• …
*In progress*
• …
*Blocked*
• …
*Decisions*
• …
```
