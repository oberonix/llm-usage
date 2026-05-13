# 5-12 Enhancements

## Task Plan

- [x] Make Help page/tab text wrap correctly across narrow and wide layouts.
- [x] Update `README.md` with first-install instructions, including a recommended install/run method that keeps the tray icon process alive.
- [x] Add a setting to start the app at login, wired into the platform startup mechanism used by the app.
- [x] Review and edit Help tab content from a new-user onboarding perspective; propose and apply clearer beginner-facing changes.
- [x] Review and edit Help tab content from an advanced/configuration/tuning perspective; propose and apply deeper usage guidance.
- [x] Add thin pacing indicators to dashboard view bars, matching the tray icon pacing concept while fitting the dashboard layout.
- [x] Investigate available Codex usage data from CLI/API/quota requests and add Codex stats below the dashboard bars using the best sourced metric, such as turns if tokens/requests are unavailable.
- [x] Investigate why Anthropic is currently grey for the 5h session and determine whether it can show the expected low percentage instead of appearing as 100% grey.

## Notes

- Codex dashboard rows now surface available local activity stats (`turns`, input tokens, output tokens) below quota bars. Live quota can use imported `chatgpt.com` cookies when configured; rollouts remain the fallback.
- Anthropic 5h quota no longer becomes grey merely because a successful OAuth response has a past/odd reset timestamp. Grey is reserved for stale cached data after the quota poll is throttled, rate-limited, or fails.
- Verification passed with `cargo check -p llm-usage-core -p llm-usage-dashboard -p llm-usage-tray` and `cargo test -p llm-usage-core -p llm-usage-dashboard -p llm-usage-tray`.
