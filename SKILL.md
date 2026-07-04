---
name: chrome-devtools-profile-control
description: Use the chrome-devtools CLI confidently to drive Chrome DevTools MCP through explicit browser profiles, persistent MCP sessions, and JSON-RPC tool calls. Trigger this skill when the user asks to operate Chrome, use Chrome DevTools MCP, open pages, inspect browser state, or perform browser UI actions through chrome-devtools.
---

# Chrome DevTools Profile Control

Use `chrome-devtools` when you need browser automation through Chrome DevTools MCP and the user expects actions to happen in a named Chrome profile.

## Core Rules

- Always choose an explicit profile with `--profile <name>`. Do not rely on an implicit browser instance.
- **Mint a session before running `mcp call` or `mcp batch`.** Both subcommands require `--session <id>` and refuse to start without it. Use `chrome-devtools session create --profile <name>` to get an id, then reuse the same id for every related invocation.
- **For multi-step automation, prefer `chrome-devtools mcp batch --profile <name> --session <id> --script <path>`.** It executes a JSON scenario through the daemon and returns a JSON array of results, which is far easier to parse than line-delimited JSON-RPC.
- For ad-hoc/interactive flows (especially `take_snapshot` → inspect uids → follow-up tool call in the same shell session), use `chrome-devtools mcp call --profile <name> --session <id>` with stdin JSON-RPC.
- Both `mcp batch` and `mcp call` route through one long-lived per-profile daemon. The daemon assigns each session to a page and injects `pageId` for page-scoped tools.
- Sessions expire after 30 minutes of inactivity. Activity (any tool call) refreshes the timer. After expiry, mint a new id.
- A session is held exclusively while bound by a `mcp call` / `mcp batch` invocation. A second concurrent invocation on the same session id is rejected by the daemon. Different session ids may run concurrently.
- `take_snapshot` result `uid` values are session uid tokens. Reuse them only in the same session and after the latest snapshot.
- Do not use `mcp direct-call` for a split `take_snapshot` then `click`/`fill` workflow; direct mode starts a separate MCP process and the snapshot cache is lost.
- Take a fresh snapshot before using `click`, `fill`, or other uid-based actions if the page may have changed.
- Do not start or kill the user's regular Chrome unless explicitly asked.
- If a browser login is required, open the page and ask the user to complete the login manually.
- The daemon and its Chrome are shared with other agents. `daemon stop` refuses while sessions are active and `profile stop` refuses while the daemon is running; only pass `--force` when the user explicitly asks to tear them down.

## Tool Argument Notes

- `navigate_page` requires a `type` discriminator: `{"type":"url","url":"...","timeout":30000}`. A bare `{"url":"..."}` fails with "Either URL or a type is required".
- If a tool fails with "The selected page has been closed", start the batch with `list_pages` and open a fresh page via `new_page` (URL argument required) instead of relying on the previously selected page.
- `fill` on a checkbox/toggle takes `value: "true"` / `"false"`. `click` and `fill` accept `includeSnapshot: true` to return a snapshot of the page right after the action.
- `new_page`'s `isolatedContext` argument is stripped automatically (with a warning): isolated contexts are incognito-like and disable extensions. When you need a separate login state, add another profile to `~/.config/chrome-devtools/config.toml` instead.
- File-writing arguments (`take_screenshot` `filePath` etc.) accept paths under the home directory and the OS tempdir.
- `get_network_request --responseFilePath <path>` saves the body with a `.network-response` extension appended to the given path.

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
chrome-devtools profile list
chrome-devtools profile status --profile conao3
```

## Session Lifecycle

Mint, reuse, and tear down sessions explicitly. Every `mcp call` and `mcp batch` invocation must carry `--session <id>`.

```sh
# Mint a session. Output: session=sess-xxxxxxxxxxxxxxxx created=<ts> last_used=<ts> owned=false
chrome-devtools session create --profile conao3

# Capture the id for later use
SESSION=$(chrome-devtools session create --profile conao3 | awk -F= '{print $2}' | awk '{print $1}')

# Inspect active sessions
chrome-devtools session list --profile conao3

# Drop a session when finished
chrome-devtools session close --profile conao3 --session "$SESSION"
```

Sessions are in-memory on the daemon. They are dropped after 30 minutes of inactivity or when the daemon stops (`chrome-devtools daemon stop --profile <name>`). After expiry, mint a new id. When a session first uses a page-scoped tool, the daemon assigns it a background page and routes later page-scoped calls to that page. `session close` and expiry close daemon-created pages when Chrome has another tab; attached pages stay open.

## Batch Execution (Preferred for Multi-Step Workflows)

`mcp batch` reads a JSON array of steps from `--script`, executes them in order through one daemon session, and prints a JSON array of per-step results. Use it whenever the steps are known up front.

### Step shapes

- Tool call: `{ "type": "tool", "name": "<mcp-tool>", "args": { ... }, "label": "<optional>", "on_error": "continue|stop" }`
- Sleep: `{ "type": "sleep_ms", "ms": <u64>, "label": "<optional>" }`

`label` is preserved verbatim in the output, which makes it easy to grep / jq for specific step results.

### Value references inside args

Replace any `args` value with `{ "$ref": "<label>.<path>" }` to substitute it with a previous step's result. `<path>` is dot-separated; numeric segments index arrays. Example: `{ "$ref": "page.result.content.0.text" }` resolves to the first content text returned by the step labelled `page`.

### Error handling

A tool step is considered to have errored if the MCP response carries a non-null `error` field or the result has `isError: true`. By default, the scenario records the error and continues; pass `--fail-fast` (or `"on_error": "stop"` on a step) to abort. The runner still emits the partial results array and exits non-zero on aborts.

When the batch fails before any step runs (e.g. `unknown session`), stdout still receives a JSON array of the shape `[{"type":"error","error":"..."}]`, so piping into `jq` keeps working; check the exit code to detect failure.

Common tool argument mistake: `navigate_page` requires a `type` discriminator, e.g. `{"type":"url","url":"...","timeout":30000}` — a bare `{"url":"..."}` fails with "Either URL or a type is required".

### Other flags

- `--script -` reads the scenario from stdin (useful with heredocs).
- `--output <path>` writes the JSON results to a file instead of stdout.

### Result shape (per step)

- Tool: `{ "type": "tool", "name": "...", "label": "...", "result": <mcp tools/call result>, "error": <mcp error or null> }`
- Sleep: `{ "type": "sleep_ms", "ms": <u64>, "label": "..." }`

### Example: navigate → wait → evaluate

```sh
cat > /tmp/scenario.json <<'JSON'
[
  { "type": "tool", "name": "navigate_page", "args": { "type": "reload", "timeout": 15000 } },
  { "type": "sleep_ms", "ms": 5000, "label": "wait-load" },
  { "type": "tool", "name": "evaluate_script", "label": "title", "args": { "function": "() => document.title" } }
]
JSON

chrome-devtools mcp batch --profile conao3 --session "$SESSION" --script /tmp/scenario.json
```

### Example: snapshot → click reusing the uid

Because all steps share the same daemon session page, a `take_snapshot` step's uid token can be referenced by a later `click` step in the same scenario.

```sh
cat > /tmp/click-scenario.json <<'JSON'
[
  { "type": "tool", "name": "take_snapshot", "args": {} },
  { "type": "tool", "name": "click", "args": { "uid": "u:abc:2:1_8" } }
]
JSON
```

When the target uid token is not known in advance, split the work: run the `take_snapshot` scenario first, parse the response, then build a second scenario that references the resolved uid token.

### Reading results with jq

```sh
chrome-devtools mcp batch --profile conao3 --session "$SESSION" --script /tmp/scenario.json \
  | jq '.[] | select(.label == "title") | .result.content[0].text'
```

## Interactive Workflow with `mcp call`

For exploratory or hand-driven JSON-RPC, `mcp call` forwards stdin lines to the daemon's MCP process and prints responses on stdout. The daemon keeps the MCP process alive across invocations.

```sh
chrome-devtools daemon start --profile conao3
SESSION=$(chrome-devtools session create --profile conao3 | awk -F= '{print $2}' | awk '{print $1}')
chrome-devtools mcp call --profile conao3 --session "$SESSION"
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

List available MCP tools:

```sh
chrome-devtools mcp list --profile conao3
```

Show MCP help:

```sh
chrome-devtools mcp help
chrome-devtools mcp batch --help
```

Inspect daemon health (version, session count, active session count, session pages, queued MCP requests, whether the daemon-attached Chrome endpoint responds):

```sh
chrome-devtools daemon status --profile conao3
# profile=conao3 daemon=ready version=0.5.0 sessions=1 active_sessions=0 pages=2 queued_mcp_requests=0 chrome=ready port=46071 pid=... socket=...
```

`chrome=unreachable` means Chrome died or moved after the daemon attached to it; every tool call will fail until the daemon is restarted.

Stop the profile daemon (refused while sessions are active; `--force` overrides):

```sh
chrome-devtools daemon stop --profile conao3
```

Bypass the daemon only for manual fallback debugging (cannot preserve snapshot uids across separate invocations, ignores sessions):

```sh
chrome-devtools mcp direct-call --profile conao3
```

Stop the Chrome instance owned by a profile (refused while the profile daemon is running; `--force` overrides):

```sh
chrome-devtools profile stop --profile conao3
```

## Troubleshooting

- If `mcp call` or `mcp batch` exits with `unknown session`, the session expired (30 min idle) or the daemon restarted. Mint a new id via `session create` and retry.
- If `mcp call` or `mcp batch` exits with `session in use`, another invocation is currently bound to that id. Wait for it to finish or mint a separate session for the new agent.
- File-writing tool arguments (`take_screenshot` `filePath` etc.) accept paths under your home directory and the OS tempdir.
- If the CLI warns about a daemon version mismatch, the daemon predates the installed CLI. Behavior fixes in the daemon only apply after it restarts; `daemon stop` (without `--force`) is safe once `sessions=0`.
- If `daemon status` shows `chrome=unreachable`, restart the daemon; it is attached to a Chrome endpoint that no longer answers.
- The daemon runs `chrome-devtools-mcp` pinned to a fixed version; set `CHROME_DEVTOOLS_MCP_VERSION` (e.g. `latest`) before `daemon start` to override.
- If `fill` or `click` fails with a stale or foreign uid token, take a fresh snapshot in the same session and use the new token.
- If the page is not the expected page, run an `evaluate_script` step that returns `window.location.href` and verify before acting.
- If the profile's Chrome is not running, `mcp call` / `mcp batch` / `mcp list` will start it on first use.
- If login state is missing, verify the profile name and `user_data_dir`; login cookies are profile-specific.
- If `mcp batch` reports `unknown step type`, check the `type` field; only `tool` and `sleep_ms` are supported.
