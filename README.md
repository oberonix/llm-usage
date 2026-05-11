# llm-usage

Lightweight menu-bar widget showing your LLM account usage and spend across
**Anthropic / OpenAI / Codex CLI / Ollama**, with hourly/weekly quota alerts.

- **Tray-only**: no Dock icon on macOS, no taskbar entry on Linux.
- **Single binary**: ~10MB stripped, ~20–30MB resident.
- **Self-contained**: no network listeners, no external services.
- **Configurable**: thresholds, budgets, model pricing — all in one TOML file.

## Status

Provider | Quota source | Spend source | Notes
---------|--------------|--------------|------
Anthropic (Pro/Max) | `GET /api/oauth/usage` w/ token from `~/.claude/.credentials.json` | `~/.claude/projects/**/*.jsonl` × per-model pricing | Same endpoint Claude Code's own `/usage` view uses. Token refresh not implemented — re-auth via Claude Code if it expires.
OpenAI API | (none — spend only) | `/v1/dashboard/billing/usage` | Unofficial endpoint; the widget surfaces "billing endpoint unavailable" if it 4xxs.
Ollama (local) | n/a | `http://localhost:11434/api/ps` | No spend (local). Live model-loaded indicator.
Codex CLI | `~/.codex/sessions/**` turn counts (5h, 7d) | reverse-engineered per-token | Best-effort; schema pinned to current Codex CLI.
Gemini CLI | `~/.gemini/tmp/<proj>/chats/*.jsonl` turn counts | reverse-engineered per-token | Best-effort; schema pinned to gemini-cli 0.41.x.
Ollama Cloud | TBD | TBD | Stub. Awaits documented usage API.

### Spend display is opt-in

By default each provider shows **quota only** — token counts, turn counts,
and any plan-quota fractions. Dollar amounts (and dollar-budget progress
bars) stay hidden until you flip `show_spend = true` under that provider's
section in `config.toml`, or tick the matching checkbox in the dashboard's
Settings tab. OpenAI is a special case: with no non-spend quota it shows
"spend tracking hidden" until enabled.

### Linking your Ollama Cloud account

Ollama publishes no usage API, so we authenticate to `/settings` with a
browser session cookie. Two ways to grab it from the dashboard's
Settings → Ollama Cloud section, neither of which requires copy-paste:

- **Sign in via popup window…** opens an embedded browser at
  `ollama.com/signin`. After you log in and land on `/settings` the cookie
  store is read automatically and saved. Requires the `llm-usage-setup`
  binary (which needs `libwebkit2gtk-4.1-dev` on Linux).
- **Import from browser…** reads the `ollama.com` cookie out of your
  already-logged-in browser (Chrome / Firefox / Edge / Brave / Safari /
  Vivaldi) via the [`rookie`](https://crates.io/crates/rookie) crate.
  Zero clicks beyond the button. On Linux you may see a one-time keyring
  prompt because Chrome's cookie DB is libsecret-encrypted.

A "Manual cookie (advanced)" collapsible is still there if you'd rather
paste the `Cookie:` header yourself.

## Build

### Linux (Pop!_OS / Ubuntu / Debian)

```bash
sudo apt install -y libgtk-3-dev libayatana-appindicator3-dev libxdo-dev \
    libwebkit2gtk-4.1-dev pkg-config libssl-dev
cargo build --release -p llm-usage-tray -p llm-usage-dashboard -p llm-usage-setup -p llm-usage
```

The four binaries land at:

- `target/release/llm-usage-tray` — the tray app (always-running)
- `target/release/llm-usage-dashboard` — opened on demand from the tray menu
- `target/release/llm-usage-setup` — one-shot login window for Ollama Cloud,
  spawned by the dashboard's "Set up login…" button
- `target/release/llm-usage` — terminal/CLI usage view. By default it
  watches the tray's shared snapshot file and redraws the quota bars
  whenever a new poll lands, so a small terminal window on the side of
  your screen mirrors the tray. Pass `--once` to render a single frame
  and exit (useful for scripts or `watch -n N`), or `--refresh` to ask
  the tray to poll right away.

### Putting `llm-usage` on `PATH`

Pick whichever you prefer. The first option doesn't need sudo and is
the most idiomatic for Rust toolchains:

```bash
# Recommended: cargo's own bin dir (already on PATH for rustup users)
cargo install --path crates/cli

# Or symlink the release build into a user-local bin dir
mkdir -p ~/.local/bin
ln -sf "$(pwd)/target/release/llm-usage" ~/.local/bin/

# If ~/.local/bin isn't on PATH yet:
echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.bashrc   # or ~/.zshrc
source ~/.bashrc

# Or system-wide (needs sudo)
sudo ln -sf "$(pwd)/target/release/llm-usage" /usr/local/bin/
```

Then anywhere in a terminal: `llm-usage`.

### macOS

```bash
cargo build --release -p llm-usage-tray -p llm-usage-dashboard -p llm-usage-setup -p llm-usage
cargo install cargo-bundle
cargo bundle --release -p llm-usage-tray
open target/release/bundle/osx/llm-usage.app
```

The bundle's `Info.plist` sets `LSUIElement=true`, so the app runs tray-only with
no Dock icon.

## Test the providers without the tray UI

If you don't have the system GUI deps yet, you can still run the data-source
side end-to-end:

```bash
cargo run -p llm-usage-core --example print_snapshots
```

This polls every enabled provider once, prints what it found, and exits.

## Config

```bash
# Linux
mkdir -p ~/.config/llm-usage
cp config.example.toml ~/.config/llm-usage/config.toml

# macOS
mkdir -p "$HOME/Library/Application Support/dev.buffbit.llm-usage"
cp config.example.toml "$HOME/Library/Application Support/dev.buffbit.llm-usage/config.toml"
```

Edit thresholds, budgets, API keys, and per-model pricing overrides as needed.
The app reads the file on each startup; restart after editing.

## Autostart

### Linux

```bash
mkdir -p ~/.config/autostart
cp packaging/linux/llm-usage.desktop ~/.config/autostart/
```

### macOS

```bash
cp packaging/macos/dev.buffbit.llm-usage.plist ~/Library/LaunchAgents/
launchctl load ~/Library/LaunchAgents/dev.buffbit.llm-usage.plist
```

## Architecture

```
crates/
  core/        # provider trait, parsers, pricing, quota engine, sqlite store
  ui/          # tray binary (tray-icon + tao + tokio runtime)
  dashboard/   # on-demand egui window (separate binary, only loaded when opened)
```

Polling runs on a tokio worker thread; the main thread is the tao event loop
(required by macOS's NSStatusItem). State updates flow main-thread-ward via
`std::sync::mpsc`. Alerts are debounced per `(provider, window, threshold)`
in SQLite so restarts don't re-fire.

## Caveats

- **Anthropic quota requires Claude Code login.** We pull `five_hour` /
  `seven_day` / `seven_day_sonnet` / `seven_day_opus` from the same
  `/api/oauth/usage` endpoint Claude Code uses, authenticated with the
  OAuth token in `~/.claude/.credentials.json`. If you've never logged
  into Claude Code on this machine, the quota line will be blank — set
  `enabled = false` for `[anthropic]` if that's permanent for you.
- **Anthropic dollar spend** is still computed from Claude Code's local
  JSONL × per-model pricing. Raw API usage made via other clients is
  not visible.
- **OpenAI's `/dashboard/billing/usage` is unofficial** and has been broken
  by OpenAI before. The widget shows "billing endpoint unavailable" if it
  returns non-2xx — usage will reappear when OpenAI fixes the endpoint.
- **Codex CLI on the ChatGPT plan has no public API.** We read the local
  `~/.codex/sessions/` rollouts; the schema is reverse-engineered and may
  break across Codex CLI releases.
- **GNOME** users will need the `AppIndicator and KStatusNotifierItem
  Support` extension. Pop!_OS COSMIC and most other desktops work out of
  the box.

## License

MIT.
