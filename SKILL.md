---
name: chrome-devtools-profile-control
description: Use the chrome-devtools CLI confidently to drive Chrome DevTools MCP through explicit browser profiles, persistent MCP sessions, and JSON-RPC tool calls. Trigger this skill when the user asks to operate Chrome, use Chrome DevTools MCP, open pages, inspect browser state, or perform browser UI actions through chrome-devtools.
---

# Chrome DevTools Profile Control

Use `chrome-devtools` when you need browser automation through Chrome DevTools MCP and the user expects actions to happen in a named Chrome profile.

## Core Rules

- Always choose an explicit profile. Do not rely on an implicit browser instance.
- Prefer `chrome-devtools mcp call --profile <name>` for real browser operations.
- `mcp call` routes through a long-lived per-profile daemon by default.
- `take_snapshot` result `uid` values are only valid inside the MCP process/session that produced them.
- The daemon keeps the MCP process alive across daemon-routed invocations, so snapshot uids may be reused while the daemon is alive.
- Do not use `mcp direct-call` for a split `take_snapshot` then `click`/`fill` workflow; direct mode starts a separate MCP process.
- Take a fresh snapshot before using `click`, `fill`, or other uid-based actions.
- Use uids only within the same MCP session that produced the snapshot.
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

## MCP Session Workflow

`take_snapshot` and subsequent uid-based `click` / `fill` calls must stay in the same MCP process. A common failure mode is taking a snapshot in one MCP process and clicking in another, which can produce `No snapshot found for page 1` because the snapshot cache is MCP-session-local. The CLI now routes `mcp call` / `mcp list` through a per-profile daemon that owns one long-lived `chrome-devtools-mcp` process.

Start or ensure the profile daemon:

```sh
chrome-devtools daemon start --profile conao3
chrome-devtools daemon status --profile conao3
```

Send MCP JSON-RPC through the daemon:

```sh
chrome-devtools mcp call --profile conao3
```

Initialize the MCP connection:

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"agent","version":"0.0.0"}}}
{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}
```

Navigate to a page:

```json
{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"navigate_page","arguments":{"type":"url","url":"https://www.google.com/","timeout":20000}}}
```

Inspect the current page:

```json
{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"take_snapshot","arguments":{}}}
```

Use snapshot uids while the same profile daemon is alive:

```json
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
```

Stop the profile daemon:

```sh
chrome-devtools daemon stop --profile conao3
```

Bypass the daemon only for manual fallback debugging:

```sh
chrome-devtools mcp direct-call --profile conao3
```

Stop a managed profile:

```sh
chrome-devtools profile stop --profile conao3
```

## Troubleshooting

- If `fill` or `click` fails with a missing snapshot, check whether the daemon restarted; take a new snapshot through the current daemon and retry.
- If the page is not the expected page, call `take_snapshot` and read the selected page URL before acting.
- If the profile is not running, `mcp call` and `mcp list` should start or reuse Chrome for the profile.
- If login state is missing, verify the profile name and `user_data_dir`; login cookies are profile-specific.
- If a DevTools port conflicts, assign a different `port` in `config.toml`.
