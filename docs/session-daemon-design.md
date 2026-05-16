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
- in-memory session metadata (id, created/last-used timestamps, owned flag);
- a background reaper that drops sessions idle for more than 30 minutes.

## Implemented behavior

1. Start one daemon per profile with `chrome-devtools daemon start --profile <name>`.
2. Keep one `chrome-devtools-mcp` process alive inside the daemon.
3. Route `chrome-devtools mcp call --profile <name> --session <id>`, `mcp batch --profile <name> --session <id> --script <path>`, and `mcp list --profile <name>` through the daemon by default.
4. Serialize client requests per profile so `take_snapshot -> click` stays inside the same MCP process and is not interleaved with another writer.
5. Keep `mcp direct-call` and `mcp direct-list` as fallbacks for simple/manual debugging.

The daemon uses a Unix domain socket under `~/.cache/chrome-devtools/daemons/<profile>.sock` and a pid file next to it. The profile-level lock under `~/.cache/chrome-devtools/locks` is held for the daemon lifetime, so direct fallback MCP commands do not run concurrently with the profile daemon.

## Session ownership

Sessions are minted, listed, and dropped over the daemon control protocol:

```text
chrome-devtools session create --profile <name>
chrome-devtools session list   --profile <name>
chrome-devtools session close  --profile <name> --session <id>
```

`mcp call` and `mcp batch` require `--session <id>`. The CLI sends `__chrome_devtools_daemon__:bind session=<id>` as the first line on its daemon connection. The daemon validates the session, marks it owned for the lifetime of that connection, and refuses concurrent binds against the same id. JSON-RPC tool calls on a bound connection refresh the session's `last_used_at` timestamp; on disconnect, the daemon releases ownership and updates `last_used_at` one more time.

A session records:

- session id (`sess-<16 hex chars>`);
- created timestamp;
- last-used timestamp;
- `owned` flag (whether a client connection currently holds it).

Sessions live in-memory on the daemon. A reaper thread wakes up every 60 seconds and drops sessions whose `last_used_at` is more than 30 minutes old and that are not currently owned. Sessions are also dropped when the daemon stops (`daemon stop` or process exit).

## Daemon control protocol

Each client connection sends a line beginning with `__chrome_devtools_daemon__:` and receives one or more newline-terminated response lines:

| Command                              | Response (per session)                                  |
|--------------------------------------|---------------------------------------------------------|
| `status`                             | `daemon=ready`                                          |
| `stop`                               | `daemon=stopping`, then daemon exits                    |
| `session_create`                     | `session=<id> created=<ts> last_used=<ts> owned=false`  |
| `session_list`                       | one line per session in the same format                 |
| `session_close session=<id>`         | `closed=<id>` or `error=<message>`                      |
| `bind session=<id>`                  | `bound=<id>` or `error=<message>`                       |

After a successful `bind`, the daemon stays in JSON-RPC forwarding mode on that connection: each JSON-RPC line is forwarded to the long-lived `chrome-devtools-mcp` child, and matching responses are streamed back.

## Future direction

- Background tab parallelism: bind multiple sessions to background browser tabs and run independent agents in parallel without sharing the active tab.
- Per-tab snapshot cache: route snapshot uids through session-scoped maps so concurrent agents do not invalidate each other's uids.
- Lock modes (`read`, `write`, `exclusive`) and origin-scoped locking for mutating operations.
- Commands must not rely on Chrome's active tab.

## Non-goals (still)

- No Chrome extension yet.
- No attempt to bypass website access controls, CAPTCHA, or login prompts.
- No BrowserContext-level isolation between sessions. Sessions today gate the daemon-level mutex and `last_used_at` timer; they do not yet partition cookies, snapshot uids, or active tab.
- No parallel writes to the same website/account.
- No implicit active-tab operations for automation flows.

## Operational rule for agents

For any action sequence that uses snapshot `uid` values, keep all related MCP calls in the same `chrome-devtools mcp call` invocation, or chain them inside one `mcp batch` script. Both bind a single session for their lifetime, so the daemon prevents another client from interleaving. Do not take a snapshot in one command invocation and click/fill in another independent MCP process.
