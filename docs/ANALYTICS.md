# Analytics & Local Quota / Model Tracking

`llm-usage` quietly records turn-level token telemetry and periodic quota snapshots into a local SQLite database (`usage.sqlite`). No telemetry leaves your machine.

---

## Database Location

| OS | Path |
|---|---|
| macOS | `~/Library/Application Support/dev.buffbit.llm-usage/usage.sqlite` |
| Linux | `~/.local/share/llm-usage/usage.sqlite` (or `$XDG_DATA_HOME/llm-usage/usage.sqlite`) |

---

## SQLite Schema

### `model_turns`
Contains deduplicated assistant turn events parsed from Claude Code logs, Codex rollouts, and OpenCode databases:
- `turn_id` (`TEXT PRIMARY KEY`): Unique turn ID (deduplicated automatically)
- `provider` (`TEXT`): Provider name (`anthropic`, `codex_cli`, `ollama_cloud`)
- `model` (`TEXT`): Exact model string (e.g. `claude-opus-4-7`, `gpt-5-codex`)
- `timestamp` (`TEXT`): ISO-8601 UTC timestamp
- `tokens_in` (`INTEGER`): Input token count
- `tokens_out` (`INTEGER`): Output token count
- `tokens_cached` (`INTEGER`): Cache read tokens
- `tokens_cache_creation` (`INTEGER`): Cache write/creation tokens

### `quota_snapshots`
Contains timestamped quota percentage readings logged on each poll:
- `id` (`INTEGER PRIMARY KEY AUTOINCREMENT`)
- `provider` (`TEXT`): Provider identifier
- `timestamp` (`TEXT`): ISO-8601 UTC timestamp
- `window_kind` (`TEXT`): Quota window (`5h`, `week`, `Sonnet`, `Opus`, etc.)
- `fraction_used` (`REAL`): Float between `0.0` and `1.0` (or `> 1.0` if exceeded)
- `resets_at` (`TEXT`): ISO-8601 UTC timestamp for when the quota window resets

---

## Querying

### Via CLI

```bash
# Model consumption breakdown
llm-usage --models

# Filter by time window (e.g. 1h, 24h, 7d, 30d) and provider
llm-usage --models --since 24h --provider anthropic

# Structured JSON format (for scripts / agents)
llm-usage --models --since 7d --json

# Chronological quota snapshot history
llm-usage --history --since 24h
llm-usage --history --provider codex_cli --json
```

### Direct SQLite Recipes

```sql
-- Model activity summary in last 24 hours
SELECT provider,
       model,
       COUNT(*) AS turns,
       SUM(tokens_in) AS in_tokens,
       SUM(tokens_out) AS out_tokens,
       SUM(tokens_cached + tokens_cache_creation) AS cached_tokens,
       SUM(tokens_in + tokens_out + tokens_cached + tokens_cache_creation) AS total_tokens
FROM model_turns
WHERE timestamp >= datetime('now', '-24 hours')
GROUP BY provider, model
ORDER BY total_tokens DESC;

-- Quota trend over time (Anthropic 5h rolling window)
SELECT timestamp, fraction_used * 100.0 AS used_pct, resets_at
FROM quota_snapshots
WHERE provider = 'anthropic' AND window_kind = '5h'
ORDER BY timestamp DESC
LIMIT 20;
```
