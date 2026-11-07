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
- On Linux with a systemd user manager, the daemon and the Chrome it launches each run in their own transient scope (`chrome-devtools-<profile>-daemon-<n>.scope` / `chrome-devtools-<profile>-chrome-<n>.scope`) via `systemd-run --user --scope`. Chrome's browser process moves itself to `app-com.google.Chrome-<pid>.scope`, but its zygote, GPU, renderer and utility processes stay in the cgroup of whatever started Chrome; giving them a scope of their own keeps them alive when the agent session that ran `daemon start-daemon` ends and its unit is stopped. Without that, Chrome cannot relaunch its GPU process and exits with `GPU process isn't usable. Goodbye.`
- Chrome's stdout and stderr go to `~/.cache/chrome-devtools/daemons/<profile>.chrome.log`. Each Chrome launch moves the previous file to `<profile>.chrome.log.1`, so the output of a Chrome that just crashed is kept after the daemon relaunches it.
- Profiles are read from `~/.config/chrome-devtools/config.toml`.
- If the config file is missing on startup, the CLI creates a `default` profile using `~/.config/chrome-devtools/profiles/default`.
- `user_data_dir` is optional; when omitted, it defaults to `~/.config/chrome-devtools/profiles/<profile-name>`.
- Prefer `user_data_dir` values under `~/.config/chrome-devtools/profiles/<profile-name>` so Chrome profile data stays with the tool config.
- The CLI may execute `chrome-devtools-mcp` internally for a selected profile.
- MCP JSON-RPC input and output are routed through one long-lived per-profile daemon by default.
- Hermes-side Chrome DevTools MCP registration is not required for this workflow.
- Daemon-routed snapshot `uid` values are session uid tokens. Keep `take_snapshot -> click/fill` on the same daemon session.
- The daemon owns one `chrome-devtools-mcp` process per profile, so multiple CLI invocations do not create competing MCP processes for the same Chrome profile. If Chrome's DevTools endpoint or MCP probe fails, the daemon respawns MCP and drops in-memory sessions so clients can mint fresh ones.
- The daemon accepts control commands independently of MCP forwarding, so `session create-session`, `session list-sessions`, `session close-session`, `daemon get-daemon-status`, and `daemon stop-daemon` respond while clients are bound.
- The daemon rewrites client JSON-RPC ids internally and restores the original id in responses, including string ids.
- `mcp forward-direct-requests` and `mcp list-direct-tools` remain available as a fallback and take a per-profile lock under `~/.cache/chrome-devtools/locks`.
- `mcp forward-requests` and `mcp run-batch` require an explicit `--session-id <id>` minted by `session create-session`. Sessions live in-memory on the daemon and expire after 30 minutes of inactivity (24 hours for a session that owns no tab); an `unknown session` error says whether the session was reaped, closed, dropped by an MCP respawn, or never existed on this daemon. Concurrent agents must mint and use their own session ids; the daemon assigns each session to a page and injects `pageId` for page-scoped tools.

## Configuration

```toml
[[profiles]]
name = "default"
```

Environment overrides:

- `CHROME_DEVTOOLS_MCP_MAX_OLD_SPACE_MB`: Node heap limit for the daemon-owned `chrome-devtools-mcp` process. Default: `2048`.
- `CHROME_DEVTOOLS_CONTROL_WARN_LATENCY_MS`: control command warn threshold. Default: `1000`.
- `CHROME_DEVTOOLS_FORWARD_WARN_LATENCY_MS`: MCP forward warn threshold. Default: `30000`.
- `CHROME_DEVTOOLS_DIAGNOSTIC_WINDOW_SECS`: `daemon get-daemon-status` latency window. Default: `600`.
- `CHROME_DEVTOOLS_CHROME_EXTRA_ARGS`: extra whitespace-separated flags appended to the Chrome command line at launch.
- `CHROME_DEVTOOLS_MCP_HEAVY_REQUEST_TIMEOUT_SECS`: request timeout for heavy tools (`take_snapshot`, `take_screenshot`, `performance_*`). Default: `300`.

Heavy-tool note: Chrome's `Accessibility.getFullAXTree` (used by `take_snapshot`) scales super-linearly with DOM size — pages with 100k+ nodes can take minutes. Timing out early does not help: chrome-devtools-mcp does not abort a cancelled tool call and holds its internal tool mutex until the call finishes, so subsequent tools of the same daemon queue behind it anyway. That is why heavy tools get a longer deadline. On very large pages prefer `evaluate_script` for targeted inspection over full snapshots.

Chrome is always launched with `--disable-features=PasswordLeakDetection`: the native "Change your password" dialog is invisible to MCP snapshots and blocks page input, which makes automated actions appear to hang. Chrome flags only apply when the daemon starts a new Chrome process — an already-running Chrome for the profile is reused as-is, so after upgrading run `chrome-devtools daemon stop-daemon --profile <name>` followed by `chrome-devtools profile stop-profile --profile <name>` (kills Chrome) once.

## Daemon-native tools

Besides forwarding the chrome-devtools-mcp tool set, the daemon implements ten tools of its own directly over CDP (they are listed in `tools/list` and called like any MCP tool). They run on the session's page without the MCP process, so they keep working while a heavy MCP tool is in flight and never trigger an accessibility snapshot:

- `goto {url, waitSelector?, waitExpression?, timeout?, interval?}`: navigate the session page without foregrounding Chrome and wait in the same call. With no explicit wait condition it waits for `document.readyState === 'complete'`. A wait that times out reports the current URL, title, `readyState` and the condition's value.
- `screenshot_quiet {filePath, format?, quality?}`: capture the visible viewport directly with `Page.captureScreenshot`, bypassing the MCP queue and leaving the user's active tab alone. `filePath` must be absolute and under `$HOME` or the OS temp directory.
- `wait_for_js {expression | selector, timeout?, interval?}`: poll a JS expression (or CSS selector) until truthy — replaces fixed sleeps for SPA waits.
- `click_selector {selector}`: scroll the first match into view and click its center. Inside iframes the click point is translated to the top frame's viewport.
- `click_text {text, exact?, index?}`: click a visible clickable element (link, button, input, ...) chosen by its visible text or aria-label, resolved in the page at call time, so no snapshot uid is needed. Top frame only; for elements inside iframes use `click_selector`. When several elements match, the error lists them with the numbers `list_clickables` shows; pass `index` to pick one.
- `list_clickables {filter?}`: list the visible clickable elements of the top frame as compact numbered lines, without a full snapshot. The numbers are the `index` values `click_text` accepts.
- `click_at {x, y}`: raw left click at viewport coordinates.
- `dispatch_key {key, modifiers?}`: single key press (Escape, Enter, arrows, single characters; modifiers bitmask Alt=1 Ctrl=2 Meta=4 Shift=8).
- `set_file_input {selector, filePaths}`: attach files to the `input[type=file]` matched by a selector through `DOM.setFileInputFiles`. No snapshot and no file chooser, so it picks the exact input on pages that have several and never crosses into another session's page. The element is resolved through `Runtime.evaluate`, so it works on pages with a large DOM (Gmail, for one). Paths must be absolute and under `$HOME` or the OS temp directory.
- `print_pdf_quiet {filePath, landscape?, printBackground?, scale?}`: save the session page as a PDF with `Page.printToPDF`, without foregrounding the tab. Use it to keep invoices, receipts and emails as they were rendered. Same path rules as `screenshot_quiet`.
- `close_page_quiet {}`: close the session's own tab through the DevTools HTTP endpoint. `close_page` queues behind the MCP tool mutex, so it cannot reach a tab that is holding the mutex open (a JavaScript dialog, a tool that never returns). Closing that tab releases the mutex, and the session picks up a fresh tab on its next call.

## JavaScript dialogs

A session's tab is watched for JavaScript dialogs from the moment the daemon assigns it. `alert` and `beforeunload` are accepted, `confirm` and `prompt` are dismissed, and each one is logged as `dialog_handled session=... target=... type=... accept=...`.

This runs on a CDP connection the daemon keeps open for the tab, because Chrome tracks a pending dialog per client: a connection that was not listening when the dialog opened gets `No dialog is showing` and cannot close it. Without the watcher, an open dialog holds the MCP tool mutex and every later request on the profile waits behind it.

If the watcher cannot attach, the daemon logs `dialog_watch_failed` and carries on; recover a stuck tab with `close_page_quiet`.
- `type_into {selector, text, clear?}`: focus the matched element and insert text with `Input.insertText`. Use it when a native value setter plus an `input` event does not reach framework state (some React-based forms).

`click_selector`, `set_file_input` and `type_into` reach into iframes with `>>>` in the selector (`iframe#form >>> input[name=email]`, nested as `iframe#a >>> iframe#b >>> input`), for same-origin and cross-origin (out-of-process) iframes alike. A missing element is reported with the frame, its URL and the iframes it contains; `could not focus selector` also says whether the element is hidden, disabled or read-only.

Each session is pinned to the CDP target id of its tab, which stays the same for the life of that tab. Navigation, redirects and SPA route changes alter the URL but not the target, tabs that share a URL are told apart (`new_page` and `select_page` pin the target id), and daemon-native tools never fall back to another tab: if the session's tab is gone they fail with `session page target <id> is gone` instead of acting on somebody else's page. This matters when several agents share one Chrome profile.

## Commands

The CLI takes input in the AWS CLI grammar: `chrome-devtools [global options] <command> <subcommand> [--parameter value ...]`. Every parameter is named, booleans come in `--flag` / `--no-flag` pairs, a value starting with `file://` is read from that file (`file:///dev/stdin` reads standard input), and the global options are `--output json|text|table` (default `json`), `--debug` and `--version`. `help` works at every level (`chrome-devtools help`, `chrome-devtools mcp help`, `chrome-devtools mcp run-batch help`).

```sh
chrome-devtools session create-session --profile default
chrome-devtools session list-sessions --profile default
chrome-devtools session close-session --profile default --session-id <id>
chrome-devtools mcp list-tools --profile default
chrome-devtools mcp run-batch --profile default --session-id <id> --steps file://steps.json
chrome-devtools mcp forward-requests --profile default --session-id <id>
chrome-devtools mcp list-direct-tools --profile default
chrome-devtools mcp forward-direct-requests --profile default
chrome-devtools daemon start-daemon --profile default
chrome-devtools daemon get-daemon-status --profile default
chrome-devtools daemon stop-daemon --profile default
chrome-devtools profile list-profiles
chrome-devtools profile get-profile-status --profile default
chrome-devtools profile stop-profile --profile default
```

Output is JSON with PascalCase keys. Exit codes: `0` success, `252` a syntax or parameter error (including an invalid step), `254` an error on the target side (unknown profile, unknown or busy session, a refused stop, an MCP error that stopped a batch), `255` any other failure.

`session create-session` starts the profile daemon when needed and mints a new in-memory session id. The session expires after 30 minutes of inactivity or when the daemon stops. Output looks like `{"SessionId": "sess-xxxxxxxxxxxxxxxx", "Created": <ts>, "LastUsed": <ts>, "Owned": false, ...}`; take the id with `jq -r .SessionId`.

`session list-sessions` prints `{"Sessions": [...]}` with one object per active session (`SessionId`, `Owned`, `OwnerPid`, `Page`, `PageUrl`, `TargetId`, ...). `session close-session` drops the named session when it is idle and closes its daemon-created page when Chrome has another tab.

`mcp list-tools` starts the profile daemon when needed, queries `tools/list` through that daemon, and prints `{"Tools": [...]}` with the MCP tool objects as MCP returns them.

`mcp forward-requests` binds the given session, forwards stdin JSON-RPC lines to the daemon-owned `chrome-devtools-mcp` process, and streams the JSON-RPC responses to stdout as they arrive (`--output` does not apply to the stream). The session is held for the lifetime of this invocation; other clients can use control commands while the bind is active, and another MCP bind waits up to `CHROME_DEVTOOLS_BIND_TIMEOUT_SECS`. Activity refreshes the session's idle timer.

`daemon get-daemon-status` includes `QueuedMcpRequests`, `MaxControlLatencyMs`, and `MaxForwardLatencyMs` for the recent diagnostic window, `Chrome` (`ready`, `unreachable` or `unknown`) with `ChromePort`, and `Respawns`, `AbnormalExits` and `LastEvent` from the health file. The default window is 600 seconds and can be changed with `CHROME_DEVTOOLS_DIAGNOSTIC_WINDOW_SECS`. When Chrome is `unreachable`, restart the daemon to reattach it.

`upload_file` first uses `chrome-devtools-mcp`. When MCP reports that direct upload and file chooser triggering both failed, the daemon sets the page's `input[type=file]` through Chrome DevTools Protocol as a fallback and logs `upload_file_fallback`.

`mcp run-batch` binds the given session and runs the `--steps` in order through the profile daemon. One `initialize` handshake is performed; then each `tool` step is issued as a `tools/call` and each `sleep_ms` step pauses the runner. Each `--steps` value is a JSON array of steps, one JSON step object, or one step in shorthand syntax, and several values are run in the order given:

```sh
chrome-devtools mcp run-batch --profile default --session-id "$SESSION" --steps \
  'Type=tool,Name=navigate_page,Args={"type":"reload"}' \
  Type=sleep_ms,Ms=5000,Label=wait-load \
  'Type=tool,Name=evaluate_script,Label=title,Args={"function":"() => document.title"}'
```

The same steps as a JSON file passed with `--steps file://steps.json`:

```json
[
  { "Type": "tool", "Name": "navigate_page", "Args": { "type": "reload" } },
  { "Type": "sleep_ms", "Ms": 5000, "Label": "wait-load" },
  { "Type": "tool", "Name": "evaluate_script", "Label": "title", "Args": { "function": "() => document.title" } }
]
```

The output is `{"Steps": [...]}` with one entry per step: `{"Type": "tool", "Name": ..., "Label": ..., "Result": <MCP tools/call result>, "Error": <MCP error or null>}` or `{"Type": "sleep_ms", "Label": ..., "Ms": ...}`. `Result` is the MCP result as MCP returns it, so the text of a tool result is `Result.content[0].text`. Select a step with `jq`, e.g. `jq -r '.Steps[] | select(.Label == "title") | .Result.content[0].text'`.

Inside `Args`, replace any value with `{"$ref":"<label>.<path>"}` to substitute it with a previous step's output entry. `<path>` is dot-separated; numeric segments index arrays. For example, `{"$ref":"snap.Result.content.0.text"}` resolves to the text of the first content entry of the step whose `Label` was `snap`.

By default a tool step that returns an error (non-null `Error` or `Result.isError: true`) is recorded and the batch continues. An `Execution context was destroyed` error (the step navigated the page) is recorded as `"Navigated": true` with `Error: null` and `Result.isError: false` instead. The steps are validated before the first one runs, so an unknown key, a `tool` step named `sleep_ms`, `sleep` or `wait`, or a step without `Type` fails with exit code 252 and the list of step types. Pass `--fail-fast` (or `"OnError": "stop"` on a step) to abort on the first error; the partial `Steps` are still printed and `chrome-devtools` exits with 254. When the batch fails before the first step (an unknown session, say), the output is `{"Steps": [], "Error": "<message>"}`, so `jq '.Steps[]'` keeps working.

Use `--steps file:///dev/stdin` to read the steps from stdin (a heredoc, say), and `--output-file <path>` to write the output to a file instead of stdout.

Important: daemon-routed `take_snapshot` result `uid` values use the form `u:<session>:<epoch>:<raw-uid>`. Pass those tokens back to later `click`/`fill` calls in the same session. Tokens from another session or an older snapshot are rejected before the request reaches MCP.

`daemon start-daemon` starts one background broker for the profile. Daemon metadata lives under `~/.cache/chrome-devtools/daemons`. `daemon stop-daemon` asks the broker to stop and cleans up its socket/pid files.

`mcp forward-direct-requests` and `mcp list-direct-tools` bypass the daemon and run `chrome-devtools-mcp` directly (while the profile daemon runs it holds the profile lock, so `list-direct-tools` lists the tools through the daemon instead). Use them only for fallback/manual debugging; they cannot preserve snapshot state across independent process invocations.

See [`docs/session-daemon-design.md`](docs/session-daemon-design.md) for the per-profile daemon/broker design and future session ownership direction.

## Development

This repository intentionally provides Rust through the Nix flake development shell. Do not assume `cargo` is installed globally.

```sh
nix develop -c cargo check
nix develop -c cargo fmt
nix develop -c cargo clippy
```
