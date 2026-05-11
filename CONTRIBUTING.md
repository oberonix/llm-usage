# Contributing to llm-usage

Thanks for considering a contribution. The project is small enough
that one good PR can move it noticeably forward.

## Before you start

For anything beyond a typo fix or one-line tweak, **please open an
issue first** so we can agree on the approach. New provider integrations
in particular have a non-obvious data shape (see the existing providers
for the patterns used) — discussing the source of truth up front saves
both sides a rebase later.

## Building

```bash
# Linux
sudo apt install -y libgtk-3-dev libayatana-appindicator3-dev libxdo-dev \
    libwebkit2gtk-4.1-dev pkg-config libssl-dev
cargo build --release \
    -p llm-usage-tray -p llm-usage-dashboard -p llm-usage-setup -p llm-usage

# macOS
cargo build --release \
    -p llm-usage-tray -p llm-usage-dashboard -p llm-usage-setup -p llm-usage
cargo install cargo-bundle --locked || true
cargo bundle --release -p llm-usage-tray
```

## Running tests

```bash
cargo test --workspace
cargo run -p llm-usage-core --example print_snapshots   # headless smoke
```

CI runs the same on every PR; see `.github/workflows/ci.yml`.

## Adding a new provider

A provider is just a `crates/core/src/providers/<name>.rs` module
implementing the `Provider` trait. Minimum surface:

- A `<Name>Provider::new(cfg)` constructor.
- An `async fn poll(&self) -> Result<UsageSnapshot>`.

Populate `snap.windows` with at least one window that has
`fraction_used: Some(_)` and `ends_at: Some(_)` if you want the
provider to participate in the tray icon's rotation. Set
`snap.plan_label` if there's a plan tier worth surfacing.

Then wire it into `crates/core/src/providers/mod.rs`, the runtime's
`build_state` in `crates/ui/src/runtime.rs`, the dashboard's
`render_status_body`, and `PROVIDER_ORDER` in `crates/ui/src/main.rs`.

## Code style

- `cargo fmt` before pushing. No extra `rustfmt.toml`; defaults are fine.
- `cargo clippy --workspace -- -D warnings` should pass.
- Comments answer **why**, not **what**. The "what" should be in the
  code; comments earn their keep by explaining a constraint, a
  reverse-engineered detail, or a non-obvious tradeoff.
- Tests for parsers live alongside the module; include a sample of
  the real upstream payload (sanitised of any session-bound tokens)
  so future-you can see what shape we expect.

## Reverse-engineered behaviour

Several providers parse data that the upstream vendor does not
officially publish (Codex CLI rate-limit snapshots, Ollama Cloud
`/settings` scrape, Anthropic `/api/oauth/usage`). These are noted at
the top of each module. When they break — and they will — open an
issue with the new payload shape and we'll iterate the parser.

## Commit messages

One-line summary; body explains why if the diff doesn't make it
obvious. Conventional Commit prefixes are welcome but not required.
