# Security policy

## Supported versions

Only the latest tagged release receives security updates. The project
is on the `0.x` line — there are no LTS branches.

## What counts as a security issue

- Anything that leaks the user's OAuth token, ollama.com session cookie,
  OpenAI API key, or any other credential to a third party.
- A path that lets an attacker read or write outside the user's own
  `~/.config/llm-usage` and `~/.local/share/llm-usage` directories
  (or the macOS equivalents).
- Network traffic to anywhere other than `api.anthropic.com`,
  `ollama.com`, `api.github.com` (only the
  `/repos/.../releases/latest` endpoint, only when the user hasn't
  disabled update notifications in Settings), or `localhost` (Ollama
  local). The app explicitly ships with no telemetry, no auto-update,
  and no listening sockets.
- A snapshot file that turns out to contain credentials.

What is *not* a security issue:

- The Anthropic OAuth `/usage` endpoint, OpenAI's billing endpoint, and
  the Ollama Cloud `/settings` scrape are all unofficial. They may
  break or change shape without notice. Their availability is a
  best-effort feature, not a security guarantee.
- The setup binary and dashboard read the user's existing browser
  cookies (`rookie`) — by design, with the user's button click. That's
  a feature, not a leak.

## How to report

Please don't open a public issue for vulnerabilities. Instead use
GitHub's private security advisory flow:

`Security` tab → `Report a vulnerability` on the repo page.

We aim to triage within 7 days, acknowledge with a CVE if applicable,
and ship a fix in the next minor release. Coordinated disclosure
within 90 days unless the bug is being actively exploited.
