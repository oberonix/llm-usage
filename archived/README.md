# Archived providers

These provider implementations are kept here for reference but are not
wired into the workspace. They were removed from the live build because
none of them currently expose a quota signal we can show on the tray
icon — only spend or activity counts — which made for a noisy menu and
a confusing icon rotation.

| File | Why archived | What it would take to re-enable |
|------|--------------|---------------------------------|
| [`providers/openai.rs`](providers/openai.rs) | OpenAI publishes no quota; only `$ spent month-to-date` via an unofficial billing endpoint. | A real OpenAI usage-quota endpoint, or a hard per-user dollar budget the user supplies and is happy to be tracked against. |
| [`providers/gemini_cli.rs`](providers/gemini_cli.rs) | Gemini CLI's JSONL session logs give us tokens and request counts but no plan-limit fraction. | A reverse-engineered fraction from `gemini-cli`'s rate-limit responses (similar to how Codex exposes `rate_limits` in its rollouts), or a published plan-limit table. |
| [`providers/ollama_local.rs`](providers/ollama_local.rs) | Local Ollama is unmetered — there's no quota to track. | If we ever surface the "live model loaded" indicator as a meaningful tray gauge, restore this and swap the fraction for a presence/idle marker. |

## How to resurrect one

1. Move the file back into `crates/core/src/providers/`.
2. Restore the corresponding `pub mod …;` and `pub use …;` lines in
   `crates/core/src/providers/mod.rs`.
3. Restore the variant on `ProviderId` (in `crates/core/src/model.rs`)
   and its `human()` and `tint_rgb()` arms.
4. Restore the `Config` struct field (in `crates/core/src/config.rs`)
   and the `Default` impl for `Config`.
5. Wire the provider into the tray's `runtime.rs` and the dashboard's
   Settings form.
6. Add an entry to `config.example.toml`.

The git history before the archive commit still shows the integrated
versions if you'd rather start from that.
