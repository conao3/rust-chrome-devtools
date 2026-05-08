# Session daemon design

## Problem

`chrome-devtools-mcp` keeps browser interaction state inside one MCP process. In practice, `take_snapshot` creates `uid` values that are only valid inside the same MCP process/session that produced the snapshot. Running `take_snapshot` in one process and `click` or `fill` in another process can fail with errors such as:

```text
No snapshot found for page 1
```

Profile-level Chrome isolation is still useful, but it is not enough for concurrent agents. Multiple Hermes agents can attach to the same Chrome profile and accidentally depend on the selected page, active tab, or the MCP process-local snapshot cache.

## Direction

Use a long-lived broker per Chrome profile.

```text
Hermes agent A \
Hermes agent B  -> chrome-devtools daemon --profile conao3 -> one Chrome profile
Hermes agent C /
```

The daemon owns:

- one Chrome process/profile/DevTools port;
- one long-lived `chrome-devtools-mcp` child process;
- per-profile concurrency control;
- future task/session metadata such as tab target ownership.

## Implemented MVP behavior

The first safe version should prefer correctness over parallelism:

1. Start one daemon per profile with `chrome-devtools daemon start --profile <name>`.
2. Keep one `chrome-devtools-mcp` process alive inside the daemon.
3. Route `chrome-devtools mcp call --profile <name>` and `mcp list` through the daemon by default.
4. Serialize client requests per profile so `take_snapshot -> click` stays inside the same MCP process and is not interleaved with another writer.
5. Keep `mcp direct-call` and `mcp direct-list` as fallbacks for simple/manual debugging.

This means the MVP is effectively:

```text
one profile -> one daemon -> one MCP process -> one active client stream at a time
```

That is intentionally conservative. It avoids active-tab and snapshot-cache races before adding background tab parallelism.

The daemon uses a Unix domain socket under `~/.cache/chrome-devtools/daemons/<profile>.sock` and a pid file next to it. The profile-level lock under `~/.cache/chrome-devtools/locks` is held for the daemon lifetime, so direct fallback MCP commands do not run concurrently with the profile daemon.

The current daemon protocol is line-oriented JSON-RPC forwarding: each client invocation sends one or more JSON-RPC lines, the daemon forwards them to the long-lived MCP child, then returns matching responses for numeric request ids. This preserves MCP process-local state across separate CLI invocations while keeping request handling serialized.

## Later behavior

After the MVP is stable, add explicit session ownership:

```text
chrome-devtools session create --profile conao3 --url https://example.com
chrome-devtools session call --session <id>
chrome-devtools session close --session <id>
```

A session should record:

- profile name;
- MCP/browser target id;
- created timestamp;
- optional owner/task id;
- current URL;
- lock mode (`read`, `write`, or `exclusive`).

Future concurrency policy:

- Browser/profile global operations stay exclusive.
- Mutating operations are locked by origin/site.
- Read-only operations may run in parallel only when they target explicit session-owned tabs.
- Commands must not rely on Chrome's active tab.

## Non-goals for the MVP

- No Chrome extension yet.
- No attempt to bypass website access controls, CAPTCHA, or login prompts.
- No parallel writes to the same website/account.
- No implicit active-tab operations for automation flows.

## Operational rule for agents

For any action sequence that uses snapshot `uid` values, keep all related MCP calls in the same `chrome-devtools mcp call` stream or use the profile daemon once available. Do not take a snapshot in one command invocation and click/fill in another independent MCP process.
