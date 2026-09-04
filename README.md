# chrome-devtools

`chrome-devtools` is a profile-aware command-line wrapper for running Chrome DevTools MCP operations with isolated Chrome user data directories.

Use it instead of raw `chrome-devtools-mcp` when multiple agents need to share one Chrome profile: the daemon separates sessions by page, keeps snapshot uids scoped to each session, and respawns Chrome/MCP when the browser endpoint or MCP process fails.

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
- Profiles own their Chrome user data directory. The daemon picks a free DevTools port at start time and records it under `~/.cache/chrome-devtools/daemons/<profile>.port`.
- Profiles are read from `~/.config/chrome-devtools/config.toml`.
- If the config file is missing on startup, the CLI creates a `default` profile using `~/.config/chrome-devtools/profiles/default`.
- `user_data_dir` is optional; when omitted, it defaults to `~/.config/chrome-devtools/profiles/<profile-name>`.
- Prefer `user_data_dir` values under `~/.config/chrome-devtools/profiles/<profile-name>` so Chrome profile data stays with the tool config.
- The CLI may execute `chrome-devtools-mcp` internally for a selected profile.
- MCP JSON-RPC input and output are routed through one long-lived per-profile daemon by default.
- Hermes-side Chrome DevTools MCP registration is not required for this workflow.
- Daemon-routed snapshot `uid` values are session uid tokens. Keep `take_snapshot -> click/fill` on the same daemon session.
- The daemon owns one `chrome-devtools-mcp` process per profile, so multiple CLI invocations do not create competing MCP processes for the same Chrome profile. If Chrome's DevTools endpoint or MCP probe fails, the daemon respawns MCP and drops in-memory sessions so clients can mint fresh ones.
- The daemon accepts control commands independently of MCP forwarding, so `session create`, `session list`, `session close`, `daemon status`, and `daemon stop` respond while clients are bound.
- The daemon rewrites client JSON-RPC ids internally and restores the original id in responses, including string ids.
- `mcp direct-call` and `mcp direct-list` remain available as a fallback and take a per-profile lock under `~/.cache/chrome-devtools/locks`.
- `mcp call` and `mcp batch` require an explicit `--session <id>` minted by `session create`. Sessions live in-memory on the daemon and expire after 30 minutes of inactivity. Concurrent agents must mint and use their own session ids; the daemon assigns each session to a page and injects `pageId` for page-scoped tools.

## Configuration

```toml
[[profiles]]
name = "default"
```

Environment overrides:

- `CHROME_DEVTOOLS_MCP_MAX_OLD_SPACE_MB`: Node heap limit for the daemon-owned `chrome-devtools-mcp` process. Default: `2048`.
- `CHROME_DEVTOOLS_CONTROL_WARN_LATENCY_MS`: control command warn threshold. Default: `1000`.
- `CHROME_DEVTOOLS_FORWARD_WARN_LATENCY_MS`: MCP forward warn threshold. Default: `30000`.
- `CHROME_DEVTOOLS_DIAGNOSTIC_WINDOW_SECS`: daemon status latency window. Default: `600`.
- `CHROME_DEVTOOLS_CHROME_EXTRA_ARGS`: extra whitespace-separated flags appended to the Chrome command line at launch.
- `CHROME_DEVTOOLS_MCP_HEAVY_REQUEST_TIMEOUT_SECS`: request timeout for heavy tools (`take_snapshot`, `take_screenshot`, `performance_*`). Default: `300`.

Heavy-tool note: Chrome's `Accessibility.getFullAXTree` (used by `take_snapshot`) scales super-linearly with DOM size — pages with 100k+ nodes can take minutes. Timing out early does not help: chrome-devtools-mcp does not abort a cancelled tool call and holds its internal tool mutex until the call finishes, so subsequent tools of the same daemon queue behind it anyway. That is why heavy tools get a longer deadline. On very large pages prefer `evaluate_script` for targeted inspection over full snapshots.

Chrome is always launched with `--disable-features=PasswordLeakDetection`: the native "Change your password" dialog is invisible to MCP snapshots and blocks page input, which makes automated actions appear to hang. Chrome flags only apply when the daemon starts a new Chrome process — an already-running Chrome for the profile is reused as-is, so after upgrading run `chrome-devtools daemon stop --profile <name>` followed by `chrome-devtools profile stop --profile <name>` (kills Chrome) once.

## Daemon-native tools

Besides forwarding the chrome-devtools-mcp tool set, the daemon implements nine tools of its own directly over CDP (they are listed in `tools/list` and called like any MCP tool). They run on the session's page without the MCP process, so they keep working while a heavy MCP tool is in flight and never trigger an accessibility snapshot:

- `goto {url, waitSelector?, waitExpression?, timeout?, interval?}`: navigate the session page without foregrounding Chrome and wait in the same call. With no explicit wait condition it waits for `document.readyState === 'complete'`.
- `screenshot_quiet {filePath, format?, quality?}`: capture the visible viewport directly with `Page.captureScreenshot`, bypassing the MCP queue and leaving the user's active tab alone. `filePath` must be absolute and under `$HOME` or the OS temp directory.
- `wait_for_js {expression | selector, timeout?, interval?}`: poll a JS expression (or CSS selector) until truthy — replaces fixed sleeps for SPA waits.
- `click_selector {selector}`: scroll the first match into view and click its center.
- `click_at {x, y}`: raw left click at viewport coordinates.
- `dispatch_key {key, modifiers?}`: single key press (Escape, Enter, arrows, single characters; modifiers bitmask Alt=1 Ctrl=2 Meta=4 Shift=8).
- `set_file_input {selector, filePaths}`: attach files to the `input[type=file]` matched by a selector through `DOM.setFileInputFiles`. No snapshot and no file chooser, so it picks the exact input on pages that have several and never crosses into another session's page. Paths must be absolute and under `$HOME` or the OS temp directory.
- `close_page_quiet {}`: close the session's own tab through the DevTools HTTP endpoint. `close_page` queues behind the MCP tool mutex, so it cannot reach a tab that is holding the mutex open (a JavaScript dialog, a tool that never returns). Closing that tab releases the mutex, and the session picks up a fresh tab on its next call.
- `type_into {selector, text, clear?}`: focus the matched element and insert text with `Input.insertText`. Use it when a native value setter plus an `input` event does not reach framework state (some React-based forms).

Each session is pinned to the CDP target id of its tab, which stays the same for the life of that tab. Navigation changes the URL but not the target, and daemon-native tools never fall back to another tab: if the session's tab is gone they fail with `session page target <id> is gone` instead of acting on somebody else's page. This matters when several agents share one Chrome profile.

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

`session list` prints one line per active session. `session close` drops the named session when it is idle and closes its daemon-created page when Chrome has another tab.

`mcp list` starts the profile daemon when needed, queries `tools/list` through that daemon, and prints the raw MCP JSON response.

`mcp call` binds the given session, forwards stdin JSON-RPC lines to the daemon-owned `chrome-devtools-mcp` process, waits for the matching JSON-RPC responses, and prints them to stdout. The session is held for the lifetime of this invocation; other clients can use control commands while the bind is active, and another MCP bind waits up to `CHROME_DEVTOOLS_BIND_TIMEOUT_SECS`. Activity refreshes the session's idle timer.

`daemon status` includes `queued_mcp_requests`, `max_control_latency_ms`, and `max_forward_latency_ms` for the recent diagnostic window. The default window is 600 seconds and can be changed with `CHROME_DEVTOOLS_DIAGNOSTIC_WINDOW_SECS`.

`upload_file` first uses `chrome-devtools-mcp`. When MCP reports that direct upload and file chooser triggering both failed, the daemon sets the page's `input[type=file]` through Chrome DevTools Protocol as a fallback and logs `upload_file_fallback`.

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

Important: daemon-routed `take_snapshot` result `uid` values use the form `u:<session>:<epoch>:<raw-uid>`. Pass those tokens back to later `click`/`fill` calls in the same session. Tokens from another session or an older snapshot are rejected before the request reaches MCP.

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
