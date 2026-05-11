---
name: Bug report
about: A reproducible defect in the tray, dashboard, CLI, or a provider parser
labels: bug
---

**What you expected**

A short sentence on what should have happened.

**What actually happened**

The wrong / surprising / broken behaviour. Include error text verbatim
where possible.

**Steps to reproduce**

1. …
2. …
3. …

**Environment**

- OS: (Pop!_OS 22.04 / macOS 14.5 / …)
- `cargo --version`:
- Which binary: tray / dashboard / setup / CLI
- Commit or release tag:

**Logs (if relevant)**

Run with `RUST_LOG=debug` and paste the last few seconds of output.
For the tray that's `RUST_LOG=debug ./target/release/llm-usage-tray`.

**Anything else**

Screenshots, hunches, suspicious provider data, etc.
