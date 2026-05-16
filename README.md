# chrome-devtools

`chrome-devtools` is a profile-aware command-line wrapper for running Chrome DevTools MCP operations with isolated Chrome user data directories.

The tool is designed to be called as a regular CLI from agent skills. It is not registered in Hermes as an MCP server.

## Agent Skill Installation

This repository is also a single-skill repository. Install the skill with the Vercel Labs `skills` CLI:

```sh
npx skills add https://github.com/conao3/rust-chrome-devtools
```

Install it for Codex explicitly:

```sh
npx skills add https://github.com/conao3/rust-chrome-devtools -a codex
```

Install it globally:

```sh
npx skills add https://github.com/conao3/rust-chrome-devtools -g
```

For local development, install from a checkout:

```sh
npx skills add ./rust-chrome-devtools
```

## Design

- Profiles are explicit: every operation that targets a browser profile requires `--profile <name>`.
- Profiles own their Chrome user data directory and DevTools port.
- Profiles are read from `~/.config/chrome-devtools/config.toml`.
- If the config file is missing on startup, the CLI creates a `default` profile using `~/.config/chrome-devtools/profiles/default`.
- `user_data_dir` is optional; when omitted, it defaults to `~/.config/chrome-devtools/profiles/<profile-name>`.
- Prefer `user_data_dir` values under `~/.config/chrome-devtools/profiles/<profile-name>` so Chrome profile data stays with the tool config.
- The CLI may execute `chrome-devtools-mcp` internally for a selected profile.
- MCP JSON-RPC input and output are routed through one long-lived per-profile daemon by default.
- Hermes-side Chrome DevTools MCP registration is not required for this workflow.
- Snapshot `uid` values are MCP-process-local. Keep `take_snapshot -> click/fill` on the same profile daemon, or in one direct MCP process when bypassing the daemon.
- The daemon owns one `chrome-devtools-mcp` process per profile, so multiple CLI invocations do not create competing MCP processes for the same Chrome profile.
- `mcp direct-call` and `mcp direct-list` remain available as a fallback and take a per-profile lock under `~/.cache/chrome-devtools/locks`.
- `mcp call` and `mcp batch` require an explicit `--session <id>` minted by `session create`. Sessions live in-memory on the daemon and expire after 30 minutes of inactivity. Concurrent agents must mint and use their own session ids so the daemon can serialize their `take_snapshot` -> `click/fill` flows independently.

## Configuration

```toml
[[profiles]]
name = "default"
port = 9222
```

## Commands

```sh
chrome-devtools session create --profile default
chrome-devtools session list --profile default
chrome-devtools session close --profile default --session <id>
chrome-devtools mcp list --profile default
chrome-devtools mcp call --profile default --session <id>
chrome-devtools mcp batch --profile default --session <id> --script batch.json
chrome-devtools mcp help
chrome-devtools daemon start --profile default
chrome-devtools daemon status --profile default
chrome-devtools daemon stop --profile default
chrome-devtools profile list
chrome-devtools profile status --profile default
chrome-devtools profile stop --profile default
```

`session create` starts the profile daemon when needed and mints a new in-memory session id. The session expires after 30 minutes of inactivity or when the daemon stops. Output looks like `session=sess-xxxxxxxxxxxxxxxx created=<ts> last_used=<ts> owned=false`.

`session list` prints one line per active session. `session close` drops the named session.

`mcp list` starts the profile daemon when needed, queries `tools/list` through that daemon, and prints the raw MCP JSON response.

`mcp call` binds the given session, forwards stdin JSON-RPC lines to the daemon-owned `chrome-devtools-mcp` process, waits for the matching JSON-RPC responses, and prints them to stdout. The session is held for the lifetime of this invocation; other clients cannot bind the same session concurrently. Activity refreshes the session's idle timer.

`mcp batch` binds the given session and reads a JSON array of steps from `--script`, running each step in order through the profile daemon. One `initialize` handshake is performed; then each `tool` step is issued as a `tools/call` and each `sleep_ms` step pauses the runner. Results are printed as a JSON array to stdout (or to `--output <path>`), so callers can inspect them programmatically without parsing line-delimited JSON-RPC. Step shapes:

```json
[
  { "type": "tool", "name": "navigate_page", "args": { "type": "reload" } },
  { "type": "sleep_ms", "ms": 5000, "label": "wait-load" },
  { "type": "tool", "name": "evaluate_script", "label": "title", "args": { "function": "() => document.title" } }
]
```

Inside `args`, replace any value with `{"$ref":"<label>.<path>"}` to substitute it with a previous step's result. `<path>` is dot-separated; numeric segments index arrays. For example, `{"$ref":"snap.result.content.0.text"}` resolves to the text of the first content entry of the result whose `label` was `snap`.

By default a tool step that returns an error (non-null `error` field or `isError: true`) is recorded and the batch continues. Pass `--fail-fast` (or `"on_error": "stop"` on a step) to abort on the first error; partial results are still written and `chrome-devtools` exits non-zero.

Use `--script -` to read the batch from stdin instead of a file, and `--output <path>` to write the JSON results to a file instead of stdout.

Important: `take_snapshot` result `uid` values are local to the running MCP process. The daemon keeps that MCP process alive for the selected profile, so a later `click`/`fill` can use snapshot state from an earlier daemon-routed invocation as long as the daemon has not restarted.

`daemon start` starts one background broker for the profile. Daemon metadata lives under `~/.cache/chrome-devtools/daemons`. `daemon stop` asks the broker to stop and cleans up its socket/pid files.

`mcp direct-call` and `mcp direct-list` bypass the daemon and run `chrome-devtools-mcp` directly. Use them only for fallback/manual debugging; they cannot preserve snapshot state across independent process invocations.

See [`docs/session-daemon-design.md`](docs/session-daemon-design.md) for the per-profile daemon/broker design and future session ownership direction.

`mcp help` prints MCP-specific usage, examples, and notes about stdio JSON-RPC forwarding.

## Development

This repository intentionally provides Rust through the Nix flake development shell. Do not assume `cargo` is installed globally.

```sh
nix develop -c cargo check
nix develop -c cargo fmt
nix develop -c cargo clippy
```
