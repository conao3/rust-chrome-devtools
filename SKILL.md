---
name: chrome-devtools-profile-control
description: Use the chrome-devtools CLI confidently to drive Chrome DevTools MCP through explicit browser profiles, persistent MCP sessions, and JSON-RPC tool calls. Trigger this skill when the user asks to operate Chrome, use Chrome DevTools MCP, open pages, inspect browser state, or perform browser UI actions through chrome-devtools.
---

# Chrome DevTools Profile Control

Use `chrome-devtools` when you need browser automation through Chrome DevTools MCP and the user expects actions to happen in a named Chrome profile.

## Core Rules

- Always choose an explicit profile with `--profile <name>`. Do not rely on an implicit browser instance.
- The CLI takes the AWS CLI grammar: `chrome-devtools <command> <subcommand> --parameter value`. Output is JSON with PascalCase keys; pick a field with `--query <JMESPath>` and print it bare with `--output text`. `help` works at every level (`chrome-devtools mcp run-batch help`). Exit code 252 is a mistake in the invocation or the steps, 254 is a target-side failure (unknown profile, unknown or busy session, refused stop, a batch stopped on an MCP error).
- **Mint a session before running `mcp run-batch` or `mcp forward-requests`.** Both subcommands require `--session-id <id>` and refuse to start without it. Use `SESSION=$(chrome-devtools session create-session --profile <name> --query SessionId --output text)` to get an id, then reuse the same id for every related invocation.
- **For multi-step automation, prefer `chrome-devtools mcp run-batch --profile <name> --session-id <id> --steps file://<path>`.** It executes the steps through the daemon and returns `{"Steps": [...]}` with one result per step, which is far easier to parse than line-delimited JSON-RPC.
- For ad-hoc/interactive flows (especially `take_snapshot` → inspect uids → follow-up tool call in the same shell session), use `chrome-devtools mcp forward-requests --profile <name> --session-id <id>` with stdin JSON-RPC.
- Both `mcp run-batch` and `mcp forward-requests` route through one long-lived per-profile daemon. The daemon assigns each session to a page and injects `pageId` for page-scoped tools.
- Sessions expire after 30 minutes of inactivity. Activity (any tool call) refreshes the timer. After expiry, mint a new id.
- A session is held exclusively while bound by a `mcp forward-requests` / `mcp run-batch` invocation. A second concurrent invocation on the same session id is rejected by the daemon. Different session ids may run concurrently.
- `take_snapshot` result `uid` values are session uid tokens. Reuse them only in the same session and after the latest snapshot.
- Do not use `mcp forward-direct-requests` for a split `take_snapshot` then `click`/`fill` workflow; direct mode starts a separate MCP process and the snapshot cache is lost.
- Take a fresh snapshot before using `click`, `fill`, or other uid-based actions if the page may have changed.
- On very large pages (huge DOM, e.g. big GitHub PR file views) `take_snapshot` can take minutes: Chrome's full accessibility-tree dump scales super-linearly with node count. Heavy tools (`take_snapshot`, `take_screenshot`, `performance_*`) get a 300s deadline (others 120s). If a heavy call times out it usually still completed in the background and blocks the next tool call until done — prefer targeted `evaluate_script` over full snapshots on such pages.
- Do not start or kill the user's regular Chrome unless explicitly asked.
- If a browser login is required, open the page and ask the user to complete the login manually.
- The daemon and its Chrome are shared with other agents. `daemon stop-daemon` refuses while sessions are active and `profile stop-profile` refuses while the daemon is running; only pass `--force` when the user explicitly asks to tear them down.
- If Chrome's DevTools endpoint or MCP stops responding, the daemon respawns MCP and drops in-memory sessions. Mint a new session after an `unknown session` response.

## Tool Argument Notes

- `navigate_page` requires a `type` discriminator: `{"type":"url","url":"...","timeout":30000}`. A bare `{"url":"..."}` fails with "Either URL or a type is required".
- If a tool fails with "The selected page has been closed", start the batch with `list_pages` and open a fresh page via `new_page` (URL argument required) instead of relying on the previously selected page.
- `fill` on a checkbox/toggle takes `value: "true"` / `"false"`. `click` and `fill` accept `includeSnapshot: true` to return a snapshot of the page right after the action.
- `new_page`'s `isolatedContext` argument is stripped automatically (with a warning): isolated contexts are incognito-like and disable extensions. When you need a separate login state, add another profile to `~/.config/chrome-devtools/config.toml` instead.
- File-writing arguments (`take_screenshot` `filePath` etc.) accept paths under the home directory and the OS tempdir.
- `get_network_request --responseFilePath <path>` saves the body with a `.network-response` extension appended to the given path.

## Daemon-Native Tools (no snapshot required)

The daemon adds ten tools of its own on top of the MCP tool set (they appear in `tools/list` and are called like any other tool). They run over CDP directly on the session's page, bypassing the MCP process — they work even while a heavy MCP tool is still running, and they never trigger an accessibility snapshot, so they are the right choice on huge-DOM pages where `take_snapshot` is impractical.

Each session is pinned to the CDP target id of its tab. Navigation, redirects, hash changes and SPA route changes alter the URL but not the target, and tabs that share a URL are told apart, because `new_page` and `select_page` pin the tab's target id when the session takes it. These tools never fall back to another tab: when the session's tab is gone they fail with `session page target <id> is gone; create a new session`, and a page that cannot be pinned fails with `could not tell which tab is the session page` (run `select_page` again or create a new session). Several agents can therefore drive one Chrome profile at the same time without stealing each other's tabs.

- `goto {url, waitSelector?, waitExpression?, timeout?, interval?}` — navigate the session page without foregrounding Chrome, then wait in the same tool call. `waitSelector` and `waitExpression` are mutually exclusive; without either it waits for `document.readyState === 'complete'`. When the condition is not met in time, the error carries the current URL, title, `readyState` and what the condition evaluated to (`wait_for_js` reports the same).
- `screenshot_quiet {filePath, format?, quality?}` — save a visible-viewport screenshot directly through CDP without selecting the page or entering the MCP queue. Use this for Slack/image viewers. `filePath` must be absolute and under `$HOME` or the OS temp directory.
- `wait_for_js {expression | selector, timeout?, interval?}` — poll a JS expression (or CSS selector) until truthy. Use instead of fixed `sleep_ms` steps for SPA rendering. Defaults: timeout 30000ms (max 120000), interval 500ms. A throwing/invalid expression fails immediately rather than polling.
- `click_selector {selector}` — scroll the first matching element into view and click its center. Inside iframes the click point is translated to the top frame's viewport.
- `click_at {x, y}` — raw left click at viewport coordinates.
- `dispatch_key {key, modifiers?}` — single key press (`Escape`, `Enter`, `Tab`, `ArrowDown`, single characters, ...). Modifiers bitmask: Alt=1, Ctrl=2, Meta=4, Shift=8.
- `set_file_input {selector, filePaths}` — attach files to the `input[type=file]` matched by the selector via `DOM.setFileInputFiles`. Prefer it over `upload_file` when a page has several file inputs (a cover letter field and a resume field, say) or when no visible button opens a chooser. Works on large-DOM pages such as an open Gmail thread. Paths must be absolute and under `$HOME` or the OS temp directory.
- `print_pdf_quiet {filePath, landscape?, printBackground?, scale?}` — save the session page as a PDF through `Page.printToPDF`. `printBackground` defaults to true, `scale` is clamped to 0.1-2.0. `filePath` must be absolute and under `$HOME` or the OS temp directory.
- `close_page_quiet {}` — close the session's own tab through the DevTools HTTP endpoint. Use it to recover when a JavaScript dialog or a tool that never returns is holding the MCP tool mutex, which is exactly when the MCP `close_page` cannot get through. The session gets a fresh tab on its next call.

The daemon watches each session's tab for JavaScript dialogs and closes them on its own: `alert` and `beforeunload` are accepted, `confirm` and `prompt` are dismissed. Look for `dialog_handled` in the daemon log. A dialog left open holds the MCP tool mutex, so this keeps a page's `confirm()` from stalling every session on the profile.
- `type_into {selector, text, clear?}` — focus the matched element and insert the text with `Input.insertText`. Use it when `evaluate_script` with a native value setter plus an `input` event leaves the framework state unchanged. `clear: true` selects all and deletes first.

`selector` of `click_selector`, `set_file_input` and `type_into` reaches into iframes with `>>>`: `iframe#form >>> input[name=email]`, nested as `iframe#a >>> iframe#b >>> input`. Same-origin and cross-origin (out-of-process) iframes both work. When an element is missing, the error names the frame, its URL and the iframes it contains (`no element matches selector: #x in frame iframe#form (url ..., iframes here: iframe#inner)`); `could not focus selector` also says whether the matched element is hidden, disabled or read-only.

## Profile Setup

Profiles are configured in `~/.config/chrome-devtools/config.toml`.

```toml
[[profiles]]
name = "default"

[[profiles]]
name = "conao3"
```

`user_data_dir` is optional. When omitted, the CLI uses:

```text
~/.config/chrome-devtools/profiles/<profile-name>
```

Check profiles before operating:

```sh
chrome-devtools profile list-profiles
chrome-devtools profile get-profile-status --profile conao3
```

## Session Lifecycle

Mint, reuse, and tear down sessions explicitly. Every `mcp run-batch` and `mcp forward-requests` invocation must carry `--session-id <id>`.

```sh
# Mint a session. Output: {"SessionId": "sess-xxxxxxxxxxxxxxxx", "Created": <ts>, "LastUsed": <ts>, "Owned": false, ...}
chrome-devtools session create-session --profile conao3

# Capture the id for later use
SESSION=$(chrome-devtools session create-session --profile conao3 --query SessionId --output text)

# Inspect active sessions ({"Sessions": [...]}; TargetId is the session's tab)
chrome-devtools session list-sessions --profile conao3

# Drop a session when finished
chrome-devtools session close-session --profile conao3 --session-id "$SESSION"
```

Sessions are in-memory on the daemon. They are dropped after 30 minutes of inactivity or when the daemon stops (`chrome-devtools daemon stop-daemon --profile <name>`). After expiry, mint a new id. When a session first uses a page-scoped tool, the daemon assigns it a background page and routes later page-scoped calls to that page. `session close-session` and expiry close daemon-created pages when Chrome has another tab; attached pages stay open.

## Batch Execution (Preferred for Multi-Step Workflows)

`mcp run-batch` runs the `--steps` in order through one daemon session and prints `{"Steps": [...]}` with one result per step. Use it whenever the steps are known up front.

### Step shapes

- Tool call: `{ "Type": "tool", "Name": "<mcp-tool>", "Args": { ... }, "Label": "<optional>", "OnError": "continue|stop" }`
- Sleep: `{ "Type": "sleep_ms", "Ms": <u64>, "Label": "<optional>" }`

Each `--steps` value is a JSON array of steps, one JSON step object, or one step in shorthand syntax (`Type=tool,Name=evaluate_script,Label=title,Args={"function":"() => document.title"}`, `Type=sleep_ms,Ms=500`). Several values run in the order given. `file://<path>` reads a value from a file and `file:///dev/stdin` from standard input (a heredoc, say).

The steps are validated before anything runs: an unknown key (a lowercase `type`, say), a `tool` step named `sleep_ms` / `sleep` / `wait` (the pause is the `sleep_ms` step type, not a tool) or a step without `Type` exits with 252 and the list of step types, and no step executes. The keys of `Args` are the MCP tool arguments and keep MCP's own names (`filePath`, `waitSelector`).

`Label` is preserved verbatim in the output, which makes it easy to select a step with `--query` or jq.

### Value references inside Args

Replace any `Args` value with `{ "$ref": "<label>.<path>" }` to substitute it with a previous step's output entry. `<path>` is dot-separated; numeric segments index arrays. Example: `{ "$ref": "page.Result.content.0.text" }` resolves to the first content text returned by the step labelled `page`.

### Error handling

A tool step is considered to have errored if its `Error` is non-null or `Result.isError` is true, except when the error is `Execution context was destroyed` (a click or script that navigated the page): that step is recorded as `"Navigated": true` with `Error: null` and `Result.isError: false`, so the batch continues and the step must not be retried. By default, the batch records the error and continues; pass `--fail-fast` (or `"OnError": "stop"` on a step) to abort. The runner still prints the partial `Steps` and exits with 254 on aborts.

When the batch fails before any step runs (e.g. `unknown session`), stdout still receives `{"Steps": [], "Error": "..."}`, so piping into `jq '.Steps[]'` keeps working; check the exit code to detect failure.

Common tool argument mistake: `navigate_page` requires a `type` discriminator, e.g. `{"type":"url","url":"...","timeout":30000}` — a bare `{"url":"..."}` fails with "Either URL or a type is required".

### Other parameters

- `--steps file:///dev/stdin` reads the steps from stdin.
- `--output-file <path>` writes the output to a file instead of stdout.
- `--fail-fast` / `--no-fail-fast` (default) choose whether an errored tool step stops the batch.

### Result shape (per step)

- Tool: `{ "Type": "tool", "Name": "...", "Label": "...", "Result": <mcp tools/call result>, "Error": <mcp error or null> }`
- Sleep: `{ "Type": "sleep_ms", "Label": "...", "Ms": <u64> }`

`Result` is the MCP result unchanged, so the text of a tool result is `Result.content[0].text`.

### Example: navigate → wait → evaluate

```sh
cat > /tmp/steps.json <<'JSON'
[
  { "Type": "tool", "Name": "navigate_page", "Args": { "type": "reload", "timeout": 15000 } },
  { "Type": "sleep_ms", "Ms": 5000, "Label": "wait-load" },
  { "Type": "tool", "Name": "evaluate_script", "Label": "title", "Args": { "function": "() => document.title" } }
]
JSON

chrome-devtools mcp run-batch --profile conao3 --session-id "$SESSION" --steps file:///tmp/steps.json
```

The same steps in shorthand, without a file:

```sh
chrome-devtools mcp run-batch --profile conao3 --session-id "$SESSION" --steps \
  'Type=tool,Name=navigate_page,Args={"type":"reload","timeout":15000}' \
  Type=sleep_ms,Ms=5000,Label=wait-load \
  'Type=tool,Name=evaluate_script,Label=title,Args={"function":"() => document.title"}'
```

### Example: snapshot → click reusing the uid

Because all steps share the same daemon session page, a `take_snapshot` step's uid token can be referenced by a later `click` step in the same batch.

```sh
chrome-devtools mcp run-batch --profile conao3 --session-id "$SESSION" --steps \
  Type=tool,Name=take_snapshot \
  'Type=tool,Name=click,Args={"uid":"u:abc:2:1_8"}'
```

When the target uid token is not known in advance, split the work: run the `take_snapshot` step first, parse the response, then run a second batch that references the resolved uid token.

### Reading results

```sh
chrome-devtools mcp run-batch --profile conao3 --session-id "$SESSION" --steps file:///tmp/steps.json \
  --query "Steps[?Label=='title'].Result.content[0].text | [0]" --output text

chrome-devtools mcp run-batch --profile conao3 --session-id "$SESSION" --steps file:///tmp/steps.json \
  | jq -r '.Steps[] | select(.Label == "title") | .Result.content[0].text'
```

## Interactive Workflow with `mcp forward-requests`

For exploratory or hand-driven JSON-RPC, `mcp forward-requests` forwards stdin lines to the daemon's MCP process and streams the JSON-RPC responses on stdout. The daemon keeps the MCP process alive across invocations.

```sh
chrome-devtools daemon start-daemon --profile conao3
SESSION=$(chrome-devtools session create-session --profile conao3 --query SessionId --output text)
chrome-devtools mcp forward-requests --profile conao3 --session-id "$SESSION"
```

Initialize the MCP connection:

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"agent","version":"0.0.0"}}}
{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}
```

Navigate / inspect / interact:

```json
{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"navigate_page","arguments":{"type":"url","url":"https://www.google.com/","timeout":20000}}}
{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"take_snapshot","arguments":{}}}
{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"fill","arguments":{"uid":"u:abc:2:1_5","value":"example text"}}}
{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"click","arguments":{"uid":"u:abc:2:1_8"}}}
```

## Common Commands

List available MCP tools (`{"Tools": [...]}`):

```sh
chrome-devtools mcp list-tools --profile conao3
chrome-devtools mcp list-tools --profile conao3 --query 'Tools[].name' --output text
```

Show help and the input shape of a subcommand:

```sh
chrome-devtools mcp help
chrome-devtools mcp run-batch help
chrome-devtools mcp run-batch --generate-cli-skeleton
```

Inspect daemon health (version, session count, active session count, session pages, queued MCP requests, recent latency maxima, whether the daemon-attached Chrome endpoint responds):

```sh
chrome-devtools daemon get-daemon-status --profile conao3
# {"Profile": "conao3", "Daemon": "ready", "Version": "1.6.0", "Sessions": 1, "ActiveSessions": 0, "Pages": [2],
#  "QueuedMcpRequests": 0, "MaxControlLatencyMs": 0, "MaxForwardLatencyMs": 42, "DiagnosticWindowSecs": 600,
#  "Chrome": "ready", "ChromePort": 46071, "Pid": ..., "Socket": ..., "AbnormalExits": 0, "Respawns": 0, "LastEvent": ...}
```

`Respawns` counts daemon-managed MCP respawns. After a respawn, mint a new session. `ChromePort` is the DevTools port of the profile's Chrome (also in `profile get-profile-status` as `Port`).

Stop the profile daemon (refused while sessions are active; `--force` overrides):

```sh
chrome-devtools daemon stop-daemon --profile conao3
```

List the tools with `mcp list-tools`; `mcp list-direct-tools` does the same through the running daemon (and says so) because the daemon holds the profile lock.

Bypass the daemon only for manual fallback debugging (cannot preserve snapshot uids across separate invocations, ignores sessions):

```sh
chrome-devtools mcp forward-direct-requests --profile conao3
```

Stop the Chrome instance owned by a profile (refused while the profile daemon is running; `--force` overrides):

```sh
chrome-devtools profile stop-profile --profile conao3
```

## Troubleshooting

- If `mcp forward-requests` or `mcp run-batch` exits with `unknown session`, the message says why the session is gone (idle reap after 30 minutes with a tab, `session close-session`, an MCP respawn, or an id this daemon never had). Mint a new id via `session create-session` and retry. A session that owns no tab is kept for 24 hours.
- If `mcp forward-requests` or `mcp run-batch` exits with `session in use: <id> (held by pid <pid> for <n>s)`, another invocation is currently bound to that id; the pid is the holding `chrome-devtools` process (`session list-sessions` shows it as `OwnerPid`). Wait for it to finish or mint a separate session for the new agent.
- File-writing tool arguments (`take_screenshot` `filePath` etc.) accept paths under your home directory and the OS tempdir.
- `upload_file` falls back to Chrome DevTools Protocol when MCP cannot trigger the file chooser; check daemon logs for `upload_file_fallback` or `upload_file_fallback_failed`.
- If the CLI warns about a daemon version mismatch, the daemon predates the installed CLI. Behavior fixes in the daemon only apply after it restarts; `daemon stop-daemon` (without `--force`) is safe once `Sessions` is 0.
- If `daemon get-daemon-status` shows `Respawns` increased, mint a new session before continuing browser work.
- The daemon runs `chrome-devtools-mcp` pinned to 1.8.0 and starts it with `--pageIdRouting`; set `CHROME_DEVTOOLS_MCP_VERSION` (e.g. `latest`) before `daemon start-daemon` to override. Up to 1.7.0 that flag is named `--experimentalPageIdRouting`, so those versions log `Unknown arguments` and fall back to their own default for page id routing.
- The daemon-owned `chrome-devtools-mcp` Node heap defaults to 2048 MB; set `CHROME_DEVTOOLS_MCP_MAX_OLD_SPACE_MB` before `daemon start-daemon` for heavier trace runs.
- If `fill` or `click` fails with a stale or foreign uid token, take a fresh snapshot in the same session and use the new token.
- If the page is not the expected page, run an `evaluate_script` step that returns `window.location.href` and verify before acting.
- If the profile's Chrome is not running, `mcp forward-requests` / `mcp run-batch` / `mcp list-tools` will start it on first use.
- When Chrome exits unexpectedly, read `~/.cache/chrome-devtools/daemons/<profile>.chrome.log.1` (the previous Chrome's output) for the `FATAL` line.
- If login state is missing, verify the profile name and `user_data_dir`; login cookies are profile-specific.
- If `mcp run-batch` reports `unknown step type`, check the `Type` field; only `tool` and `sleep_ms` are supported.
