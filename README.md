# llm-usage

**A menu-bar gauge for your AI coding quotas.** At a glance, see how
close you are to your Anthropic, Codex, and Ollama Cloud limits — and
get a desktop alert *before* you hit the wall mid-task, not after.

If you live in Claude Code or the Codex CLI, you've felt the surprise
of a five-hour or weekly cap landing in the middle of something. This
is a tiny resident tray icon that turns that surprise into a glance:
coloured quota bars, a pacing marker that shows whether you're burning
faster than the clock, and threshold alerts you configure once.

It reads the **same local credentials and usage signals your tools
already use** — nothing leaves your machine except the calls to your
own providers (and one optional, off-by-default update check).

---

## Highlights

- **At-a-glance tray icon.** Rotating per-provider quota bars right in
  the menu bar / system tray. Green → amber → red as you approach a
  cap, with a magenta pacing marker showing elapsed-time vs. usage so
  you can tell "comfortably ahead" from "about to run out".
- **Multi-provider.** Anthropic (Pro/Max), Codex CLI (ChatGPT plan),
  and Ollama Cloud — each with the windows that matter (5-hour,
  weekly, and activity rollups).
- **Alerts before the wall.** Pick the fractions you care about
  (`0.5, 0.75, 0.9`, …); get one desktop notification per threshold
  per window. Debounced in SQLite so a restart never re-spams you.
- **Privacy by construction.** No telemetry, no analytics, no
  auto-update, no listening sockets, no background phone-home. The
  only non-provider call is an optional once-a-day GitHub release
  check that ships **off**.
- **Spend is opt-in.** By default you see quotas only. Dollar amounts
  and budget bars stay hidden until you explicitly turn them on,
  per provider.
- **Terminal companion.** A `llm-usage` CLI mirrors the tray in any
  terminal — great for a tmux pane or `watch`.
- **One config file**, editable from a built-in dashboard: thresholds,
  budgets, poll cadence, per-model pricing overrides.
- **Tray-only.** No Dock icon on macOS, no taskbar entry on Linux.

## Supported providers

| Provider | Quota signal | Spend (opt-in) | Source of truth |
|---|---|---|---|
| **Anthropic** (Pro/Max) | 5h / weekly / weekly-Sonnet / weekly-Opus | local JSONL × per-model pricing | `GET /api/oauth/usage` using the OAuth token in `~/.claude/.credentials.json` — the same endpoint Claude Code's own `/usage` view uses |
| **Codex CLI** (ChatGPT plan) | 5h / weekly + 1h/today/month activity | reverse-engineered per-token estimate | `~/.codex/sessions/**` rollouts; optional live quota via a saved chatgpt.com cookie |
| **Ollama Cloud** | plan usage from the account page | — (no usage API) | `ollama.com/settings` scraped with a saved browser session cookie |

> **Best-effort by design.** None of these sources is an official,
> documented usage API — they're the same local files and endpoints
> your tools use, parsed by us. Upstream can change shape without
> notice; when it does the affected provider shows "parse failed"
> until the parser is updated. Open an issue with the new payload
> shape and we'll iterate.

Earlier OpenAI-API, Gemini-CLI, and local-Ollama integrations were
removed from the build because none exposed a real quota fraction
(only spend or activity), which made for a noisy tray. They're kept,
documented, and resurrectable under [`archived/`](archived/).

## Install

### From a release (recommended)

Grab the latest build from the
[**Releases**](https://github.com/oberonix/llm-usage/releases) page.

**Linux (x86_64):**

```bash
tar xf llm-usage-*-linux-x86_64.tar.xz
cd llm-usage-*-linux-x86_64
sudo cp llm-usage{,-tray,-dashboard,-setup} /usr/local/bin/   # or ~/.local/bin/
./llm-usage-tray &                                            # start the tray
```

**macOS (Intel = `x86_64`, Apple Silicon = `aarch64`):**

```bash
unzip llm-usage-*-macos-*.zip
mv llm-usage-*-macos-*/llm-usage.app /Applications/
xattr -d com.apple.quarantine /Applications/llm-usage.app   # see note below
open /Applications/llm-usage.app
```

> Release binaries are **unsigned**. macOS Gatekeeper refuses unsigned
> apps on first launch; the `xattr` line tells it you've reviewed the
> binary. Every release ships a `.sha256` next to each archive so you
> can verify the download. Prefer not to run unsigned binaries? Build
> from source instead.

Then open **Settings** from the tray menu and turn on **Start tray at
login** so the icon comes back after you sign in.

### From source

Requires a recent stable Rust toolchain.

**Linux (Pop!\_OS / Ubuntu / Debian):**

```bash
sudo apt install -y libgtk-3-dev libayatana-appindicator3-dev libxdo-dev \
    libwebkit2gtk-4.1-dev pkg-config libssl-dev
cargo build --release \
    -p llm-usage-tray -p llm-usage-dashboard -p llm-usage-setup -p llm-usage
./target/release/llm-usage-tray
```

**macOS:**

```bash
cargo build --release \
    -p llm-usage-tray -p llm-usage-dashboard -p llm-usage-setup -p llm-usage
cargo install cargo-bundle --locked
cargo bundle --release -p llm-usage-tray
open target/release/bundle/osx/llm-usage.app
```

The four binaries are:

| Binary | Role |
|---|---|
| `llm-usage-tray` | The resident tray app — keep this running. Does the polling, owns the menu-bar icon, fires alerts, writes the shared snapshot. |
| `llm-usage-dashboard` | On-demand window opened from the tray menu. Quota bars, history, and the Settings form. |
| `llm-usage-setup` | One-shot login window for capturing the Ollama Cloud cookie; spawned by the dashboard. |
| `llm-usage` | Terminal view that mirrors the tray. `--once` for a single frame (scripts / `watch`); `--refresh` to force a poll. |

The dashboard and CLI are **companion views** — they don't replace the
tray. Keep `llm-usage-tray` running for the icon, polling, and alerts.

To put the CLI on `PATH`: `cargo install --path crates/cli`, or symlink
`target/release/llm-usage` into `~/.local/bin/`.

### Try it without the GUI

No system GUI deps installed? You can still exercise the whole
data-source side:

```bash
cargo run -p llm-usage-core --example print_snapshots
```

This polls every enabled provider once, prints what it found, exits.

## Configuration

Copy the example config into place and edit it (the dashboard's
Settings tab writes the same file, so most people never touch it by
hand):

```bash
# Linux
mkdir -p ~/.config/llm-usage
cp config.example.toml ~/.config/llm-usage/config.toml

# macOS
mkdir -p "$HOME/Library/Application Support/dev.buffbit.llm-usage"
cp config.example.toml "$HOME/Library/Application Support/dev.buffbit.llm-usage/config.toml"
```

Thresholds, budgets, poll cadence, and per-model pricing overrides all
live there. The tray reads it on startup; restart after a hand-edit
(or just use the dashboard, which applies changes live).

### Spend display is opt-in

Every provider shows **quota only** by default — token/turn counts and
plan-quota fractions. Dollar amounts and budget bars stay hidden until
you flip `show_spend = true` for that provider (or tick its checkbox in
Settings).

### Linking your Ollama Cloud account

Ollama publishes no usage API, so the app reads your logged-in
`ollama.com/settings` page with a browser session cookie. Two
one-click ways to capture it from **Settings → Ollama Cloud**, neither
needing copy-paste:

- **Import from browser…** — reads the `ollama.com` cookie from a
  browser you're already logged into (Chrome / Firefox / Edge / Brave /
  Safari / Vivaldi) via the [`rookie`](https://crates.io/crates/rookie)
  crate. On Linux you may get a one-time keyring prompt because
  Chrome's cookie DB is libsecret-encrypted.
- **Sign in via popup window…** — opens an embedded browser at
  `ollama.com/signin`; once you land on `/settings` the cookie is
  saved automatically. Needs `llm-usage-setup` (and
  `libwebkit2gtk-4.1-dev` on Linux).

There's also a "Manual cookie (advanced)" field if you'd rather paste
the `Cookie:` header yourself. The cookie expires every few weeks; the
dashboard surfaces a "session likely expired" hint so you know when to
re-capture.

### Autostart

The dashboard's **Start tray at login** toggle is the recommended
path — it writes the platform login item pointing at your actual
`llm-usage-tray` binary. The equivalents by hand:

```bash
# Linux
mkdir -p ~/.config/autostart
sed "s#Exec=llm-usage-tray#Exec=$(pwd)/target/release/llm-usage-tray#" \
    packaging/linux/llm-usage.desktop > ~/.config/autostart/llm-usage.desktop

# macOS
cp packaging/macos/dev.buffbit.llm-usage.plist ~/Library/LaunchAgents/
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/dev.buffbit.llm-usage.plist
```

## Privacy — what leaves your machine

This is the whole point, so it's worth stating plainly:

- **Outbound traffic** goes only to your own providers:
  `api.anthropic.com`, `chatgpt.com` (only if you've supplied a Codex
  cookie), and `ollama.com` (only if you've linked Ollama Cloud).
- **The one exception** is an optional GitHub Releases check
  (`api.github.com`, the `releases/latest` endpoint, at most once a
  day). It ships **off** (`check_for_updates`) and is a single
  Settings toggle.
- **No telemetry, no analytics, no auto-update, no listening
  sockets.** Your credentials are read locally and used only to talk
  to the provider they belong to.
- The shared `snapshots.json` the tray writes for the dashboard/CLI
  contains usage counts and percentages **only** — never tokens,
  cookies, or keys.

See [`SECURITY.md`](SECURITY.md) for the threat model and how to
report an issue.

## Caveats

- **Anthropic quota needs a Claude Code login on this machine.** It
  reuses the OAuth token in `~/.claude/.credentials.json`. No token,
  no quota line — set `enabled = false` for `[anthropic]` if that's
  permanent for you. Token refresh isn't implemented; if it expires,
  re-auth via Claude Code. Dollar spend is computed from Claude Code's
  local JSONL × per-model pricing (plus opencode's SQLite store,
  filtered to the `anthropic` providerID, if you use it).
- **Codex CLI on the ChatGPT plan has no public API.** Quota comes
  from the local `~/.codex/sessions/` rollouts (schema
  reverse-engineered, may break across Codex releases) or, if you've
  supplied a chatgpt.com cookie, the live quota endpoint. opencode's
  per-message store is folded in for users who drive OpenAI through
  opencode.
- **Ollama Cloud** is a session-cookie scrape of `/settings`; the
  cookie expires every few weeks (the dashboard tells you when).
- **GNOME** needs the *AppIndicator and KStatusNotifierItem Support*
  extension. Pop!\_OS COSMIC and most other desktops work out of the
  box.

## Architecture

```
crates/
  core/        # provider trait, parsers, pricing, quota engine, sqlite store
  ui/          # tray binary (tray-icon + tao + tokio runtime)
  dashboard/   # on-demand egui window (separate binary, loaded only when opened)
  setup/       # one-shot Ollama Cloud cookie-capture window
  cli/         # terminal companion view
```

Polling runs on a tokio worker thread; the main thread is the tao
event loop (required by macOS's `NSStatusItem`). State flows
main-thread-ward via `std::sync::mpsc`. Alerts are debounced per
`(provider, window, threshold)` in SQLite so restarts don't re-fire.

## Contributing

PRs welcome — see [`CONTRIBUTING.md`](CONTRIBUTING.md). New provider
integrations have a non-obvious data shape, so please open an issue to
agree on the source of truth before a big PR.

## License

[MIT](LICENSE).
