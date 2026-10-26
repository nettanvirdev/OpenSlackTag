# Security

OpenSlackTag runs tools with your keys, so security reports are welcome.

**Please report vulnerabilities privately**, not in a public issue: use GitHub's *Report a vulnerability* button on the [Security tab](https://github.com/nettanvirdev/OpenSlackTag/security). Include what you found, how to reproduce it, and what an attacker could do with it. You'll get a reply within a week.

Things worth reporting:
- reading or writing files outside a conversation's workspace
- web fetches reaching private or local addresses
- a tool running without the approval its rules require, or an approval accepted from the wrong person
- one person's routine or trigger acting with someone else's access
- Slack messages that ping `@channel`, `@here` or user groups
- secrets showing up in logs, the audit log or the wiki

The [Safety section of the README](README.md#safety) describes what OpenSlackTag is designed to prevent.
