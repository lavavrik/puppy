# puppy — scriptable CLI commands (short design)

Date: 2026-11-20 · Status: approved in conversation

## Intent
Use puppy without the dashboard: typed by hand, from scripts/cron, and by other tools handing downloads over. Readable by people by default, machine-readable on request, with exit codes scripts can rely on.

## Approach
New clap subcommands in the existing `puppy` binary, thin over `puppy-client` (the daemon already supports every operation). Rejected: a separate binary (no benefit), a raw JSON-RPC passthrough (scripts would depend on protocol internals).

## Commands
```
puppy add <SOURCE>... [-o DIR] [--name FILE] [--paused] [--limit RATE]
          [--header "K: V"]... [--cookie STR] [--referer URL] [--wait]
          (SOURCE "-" reads one source per line from stdin)
puppy ls [--state S[,S...]]
puppy info <ID>
puppy pause|resume|restart <ID...|all>
puppy rm <ID...> [--delete-files]
puppy move <ID> <POSITION>
puppy limit <ID> <RATE|none>
puppy limit --global [--down RATE|none] [--up RATE|none]
puppy max-active <N>
puppy wait <ID...>
puppy watch
puppy stop
puppy status            (unchanged)
```
RATE uses the dashboard's parser (`2M`, `500k`, `none`).

## Output
- Every command accepts `--json`; JSON is the protocol's own types (DownloadSnapshot, EngineDetail, Event…) serialized as-is — versioned via the protocol.
- Human output to stdout (aligned tables / short confirmations); progress and errors to stderr. Without a TTY no progress lines.
- `watch --json` emits one JSON event per line (NDJSON).

## Exit codes
0 ok · 1 error (daemon/operation) · 2 usage · 3 unknown id · 4 waited download failed · 5 waited download needs attention.

## Behaviour
- Every command auto-spawns the daemon like the dashboard (`connect_or_spawn`), except `stop` (no daemon ⇒ "not running", exit 0).
- `add --wait` / `wait`: block until every listed download is Completed/Failed/NeedsAttention; live single-line progress on a TTY; exit code per the table (worst outcome wins).
- `pause|resume|restart all` acts on every download for which the action is valid.
- Multiple IDs: act on each, report each failure, exit with the worst code.

## Non-goals
Shell completions, config editing commands, interactive prompts.

## Left to the builder
Table layout details, module split inside `crates/puppy`, how stdin sources are trimmed/commented (`#` lines skipped is fine).
