---
name: chrome-devtools-profile-control
description: Use the chrome-devtools CLI confidently to drive Chrome DevTools MCP through explicit browser profiles, persistent MCP sessions, and JSON-RPC tool calls. Trigger this skill when the user asks to operate Chrome, use Chrome DevTools MCP, open pages, inspect browser state, or perform browser UI actions through chrome-devtools.
---

# Chrome DevTools Profile Control

Use `chrome-devtools` when you need browser automation through Chrome DevTools MCP and the user expects actions to happen in a named Chrome profile.

## Core Rules

- Always choose an explicit profile with `--profile <name>`. Do not rely on an implicit browser instance.
- **For multi-step automation, prefer `chrome-devtools mcp exec --profile <name> --script <path>`.** It executes a JSON scenario through the daemon and returns a JSON array of results, which is far easier to parse than line-delimited JSON-RPC.
- For ad-hoc/interactive flows (especially `take_snapshot` → inspect uids → follow-up tool call in the same shell session), use `chrome-devtools mcp call --profile <name>` with stdin JSON-RPC.
- Both `mcp exec` and `mcp call` route through one long-lived per-profile daemon, so `take_snapshot` uids returned in step N can be reused in step N+1 within the same scenario or the same `mcp call` invocation.
- `take_snapshot` result `uid` values are MCP-session-local. They stay valid only while the same daemon-owned MCP process is alive.
- Do not use `mcp direct-call` for a split `take_snapshot` then `click`/`fill` workflow; direct mode starts a separate MCP process and the snapshot cache is lost.
- Take a fresh snapshot before using `click`, `fill`, or other uid-based actions if the page may have changed.
- Do not start or kill the user's regular Chrome unless explicitly asked.
- If a browser login is required, open the page and ask the user to complete the login manually.

## Profile Setup

Profiles are configured in `~/.config/chrome-devtools/config.toml`.

```toml
[[profiles]]
name = "default"
port = 9222

[[profiles]]
name = "conao3"
port = 9223
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

## Scenario Execution (Preferred for Multi-Step Workflows)

`mcp exec` reads a JSON array of steps from `--script`, executes them in order through one daemon session, and prints a JSON array of per-step results. Use it whenever the steps are known up front.

### Step shapes

- Tool call: `{ "type": "tool", "name": "<mcp-tool>", "args": { ... }, "label": "<optional>", "on_error": "continue|stop" }`
- Sleep: `{ "type": "sleep_ms", "ms": <u64>, "label": "<optional>" }`

`label` is preserved verbatim in the output, which makes it easy to grep / jq for specific step results.

### Value references inside args

Replace any `args` value with `{ "$ref": "<label>.<path>" }` to substitute it with a previous step's result. `<path>` is dot-separated; numeric segments index arrays. Example: `{ "$ref": "page.result.content.0.text" }` resolves to the first content text returned by the step labelled `page`.

### Error handling

A tool step is considered to have errored if the MCP response carries a non-null `error` field or the result has `isError: true`. By default, the scenario records the error and continues; pass `--fail-fast` (or `"on_error": "stop"` on a step) to abort. The runner still emits the partial results array and exits non-zero on aborts.

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

chrome-devtools mcp exec --profile conao3 --script /tmp/scenario.json
```

### Example: snapshot → click reusing the uid

Because all steps share the same daemon-owned MCP session, a `take_snapshot` step's uid can be referenced by a later `click` step in the same scenario.

```sh
cat > /tmp/click-scenario.json <<'JSON'
[
  { "type": "tool", "name": "take_snapshot", "args": {} },
  { "type": "tool", "name": "click", "args": { "uid": "1_8" } }
]
JSON
```

When the target uid is not known in advance, split the work: run the `take_snapshot` scenario first, parse the response, then build a second scenario that references the resolved uid.

### Reading results with jq

```sh
chrome-devtools mcp exec --profile conao3 --script /tmp/scenario.json \
  | jq '.[] | select(.label == "title") | .result.content[0].text'
```

## Interactive Workflow with `mcp call`

For exploratory or hand-driven JSON-RPC, `mcp call` forwards stdin lines to the daemon's MCP process and prints responses on stdout. The daemon keeps the MCP process alive across invocations.

```sh
chrome-devtools daemon start --profile conao3
chrome-devtools mcp call --profile conao3
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
{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"fill","arguments":{"uid":"1_5","value":"example text"}}}
{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"click","arguments":{"uid":"1_8"}}}
```

## Common Commands

List available MCP tools:

```sh
chrome-devtools mcp list --profile conao3
```

Show MCP help:

```sh
chrome-devtools mcp help
chrome-devtools mcp exec --help
```

Stop the profile daemon:

```sh
chrome-devtools daemon stop --profile conao3
```

Bypass the daemon only for manual fallback debugging (cannot preserve snapshot uids across separate invocations):

```sh
chrome-devtools mcp direct-call --profile conao3
```

Stop the Chrome instance owned by a profile:

```sh
chrome-devtools profile stop --profile conao3
```

## Troubleshooting

- If `fill` or `click` fails with a missing snapshot, check whether the daemon restarted between the snapshot and the action. Re-snapshot inside the same `mcp exec` scenario, or inside one `mcp call` invocation.
- If the page is not the expected page, run an `evaluate_script` step that returns `window.location.href` and verify before acting.
- If the profile's Chrome is not running, `mcp call` / `mcp exec` / `mcp list` will start it on first use.
- If login state is missing, verify the profile name and `user_data_dir`; login cookies are profile-specific.
- If a DevTools port conflicts, assign a different `port` in `config.toml`.
- If `mcp exec` reports `unknown step type`, check the `type` field; only `tool` and `sleep_ms` are supported.
