# Tab-parallel session design

## Problem

The current daemon gives each client a session id, but MCP forwarding still has one profile-wide bind slot. This protects `take_snapshot -> click/fill` flows, but it also means one bound client blocks every other MCP client for that profile.

The next step is to let multiple sessions stay active on the same Chrome profile while each session keeps its own tab and snapshot uid namespace. The design target is concurrent agent work through session-owned tabs instead of Chrome's selected page state.

## Chrome DevTools MCP 1.5.0 facts

The checked package is `chrome-devtools-mcp` 1.5.0 under `~/.npm/_npx/15c61037b1978c83/node_modules/chrome-devtools-mcp/`.

`build/src/bin/chrome-devtools-mcp-cli-options.js` defines `experimentalPageIdRouting`. The option description says it exposes `pageId` on page-scoped tools and routes requests by page ID.

`build/src/ToolHandler.js` adds `pageId` to page-scoped tools when `experimentalPageIdRouting` is enabled. During a tool call, it resolves the target page with `context.getPageById(pageId)` instead of `context.getSelectedMcpPage()`.

`build/src/tools/script.js` gives `evaluate_script` the same `pageId` path when `experimentalPageIdRouting` is enabled. That tool is defined outside `definePageTool`, so this separate path matters.

`tools/list` confirms the schema change. With `--experimentalPageIdRouting`, tools such as `click`, `fill`, `navigate_page`, `take_snapshot`, `take_screenshot`, `wait_for`, `list_console_messages`, and `list_network_requests` require `pageId`.

`build/src/McpContext.js` keeps one global `#selectedPage`. `newPage(background, isolatedContextName)` creates a Puppeteer page, refreshes the page snapshot, and calls `selectPage(newPage)`. `select_page` also mutates the same selected page.

`McpContext.createPagesSnapshot()` assigns numeric page ids via `new McpPage(page, this.#nextPageId++)`. It also calls `page.emulateFocusedPage(true)` for every known page.

`McpPage` stores `textSnapshot` per page. `getElementByUid(uid)` looks up the uid in that page's latest snapshot. With page id routing, the page id chooses the `McpPage`, and the uid lookup stays page-local.

`ToolHandler` uses one `toolMutex` for all tools. A single `chrome-devtools-mcp` process still runs one tool handler at a time even when page id routing is enabled.

## Direction

Run the daemon-owned MCP process with `--experimentalPageIdRouting`.

```text
Agent A session=sess-a -> daemon router -> MCP tools/call pageId=1 -> Chrome tab 1
Agent B session=sess-b -> daemon router -> MCP tools/call pageId=2 -> Chrome tab 2
Agent C session=sess-c -> daemon router -> MCP tools/call pageId=3 -> Chrome tab 3
```

The daemon owns the mapping from session id to page id. Clients keep using `chrome-devtools mcp call --session <id>` and `mcp batch --session <id>`; the daemon injects `pageId` internally.

The router becomes the session adapter:

- allocate or attach a page for the session;
- inject the session's `pageId` into page-scoped tool calls;
- translate session uid tokens to raw MCP uids;
- rewrite snapshot responses so returned uids are session-scoped;
- treat upstream selected page text as diagnostic state.

## Session page ownership

Extend `SessionState` with page ownership fields:

```text
page_id=<mcp page id or empty>
page_created_by_daemon=<true|false>
page_url=<last observed url or empty>
target_id=<cdp target id or empty>
snapshot_epoch=<u64>
```

`page_id` is the MCP page id, which `McpContext.createPagesSnapshot()` re-issues from `#nextPageId++`, so it shifts when other tabs open or close. `target_id` is the CDP target id from `/json`, which stays the same for the life of the tab. Daemon-native tools resolve their connection through `target_id` and treat `page_url` as diagnostic state.

The daemon learns the target id by diffing `/json` page target ids around the `new_page` call in `ensure_session_page`. A session that has a page but no target id (after an MCP respawn, say) resolves it once from `page_url`, and only when exactly one target carries that url.

`connect_page_client` never falls back to another target. A missing target returns `session page target <id> is gone; create a new session`, and an ambiguous url returns `url <url> matches multiple page targets`. Falling back to `targets.first()` sends input to whatever tab happens to be first, which on a shared profile is another agent'"'"'s page.

A session can get a page in 3 ways.

1. Lazy allocation: the first page-scoped tool call opens `about:blank` as a background tab, records its page id, then applies the original tool call to that page.
2. Explicit creation: a client `new_page` call opens a background tab with the requested URL and records the returned page id for that session.
3. Explicit attachment: a daemon control command records an existing page id for the session.

Page allocation uses Chrome's default browser context. `isolatedContext` is reserved for an explicit separate storage context request. The daemon's session isolation is page-id based, so regular profile extensions stay available in session tabs.

`new_page` defaults to `background=true` in session-routed mode. A client may request foreground focus. Routing still uses the session page id.

## Router behavior

The router already owns the MCP stdin/stdout pair and rewrites JSON-RPC ids. Tab-parallel sessions extend that same point.

For each `tools/call` request, the router looks at `params.name`:

| Tool kind | Router behavior |
|-----------|-----------------|
| Page-scoped tool | Ensure the session has a page, insert `params.arguments.pageId=<session page id>`, translate uid fields, then forward. |
| `evaluate_script` | Same as page-scoped tools because MCP 1.5.0 has a separate `pageId` path for it. |
| `new_page` | Open a default-context page, record the new page id for the session, and return the updated page list. |
| `select_page` | Update the daemon's session-to-page mapping. The upstream selected page is left as diagnostic state. |
| `list_pages` | Return page data with the daemon's session page marked as the session target. |
| `close_page` | Close the caller session's page. A later policy may accept an explicit page id. |
| Extension and profile-level tools | Run under an explicit policy because they target profile-level state. |

The router should keep `tools/list` client-friendly. If the daemon starts MCP with `--experimentalPageIdRouting`, upstream `tools/list` marks `pageId` as required on many tools. The daemon should remove `pageId` from the schema it returns to clients and keep `pageId` injection as an internal detail.

## UID namespace

Raw MCP uids are scoped by `McpPage.textSnapshot`, but clients only see strings. The daemon should make that scope explicit.

When a response contains snapshot uids, the router rewrites them into session uid tokens:

```text
u:<session-short-id>:<snapshot-epoch>:<raw-uid>
```

The session registry stores the reverse map:

```text
session uid token -> page id, snapshot epoch, raw uid
```

When a later tool call contains uid parameters, the router translates each session uid token back to the raw uid and injects the owning page id. The translation accepts only tokens owned by the calling session and current session page.

The first implementation should handle the uid-bearing fields exposed by MCP 1.5.0:

- `uid`;
- `from_uid`;
- `to_uid`;
- `elements[].uid`;
- `args[]` for `evaluate_script`.

Each `take_snapshot` response replaces the session's uid map for that page. Tool calls that return a fresh snapshot through `includeSnapshot` apply the same rewrite.

Unknown, stale, or cross-session uid tokens return a daemon error before forwarding to MCP. That keeps wrong-page actions from reaching Chrome.

## Selected page policy

The daemon treats MCP's selected page as global MCP state. Session routing uses the daemon's `session -> pageId` map instead.

Page-scoped tool calls always get `pageId`. `select_page` becomes a session assignment operation. `list_pages` reports the session page as the active target for the caller.

Upstream MCP can still mutate `#selectedPage` during `new_page` and raw `select_page`. That mutation is harmless for routed calls because page-scoped calls bypass `getSelectedMcpPage()`.

A daemon debug mode can expose both values:

```text
upstream_selected_page=<id>
session_page=<id>
```

Regular client output should present the session page as the selected page so agent prompts stay simple.

## Execution model

Multiple daemon client connections can be bound to different sessions at the same time. The session registry uses per-session ownership.

The router still serializes writes to the single MCP process. MCP 1.5.0 also has a process-wide `toolMutex`, so one long-running tool call delays later tool calls. Tab parallelism gives state isolation first: sessions can stay active, hold their own page ids, and keep uid maps while preserving each session's page and uid state.

A later design can split read-only calls or run multiple MCP child processes against the same Chrome profile. That requires separate proof for page ids, DevTools event collectors, trace state, and snapshot lifetimes.

## Page lifecycle

A session-owned page is idle-managed with the session.

- When the session expires, pages created by the daemon are closed when Chrome has another tab.
- Pages attached from an existing tab stay open when the session expires.
- `session close` closes daemon-created pages when they are idle.
- `daemon stop` keeps Chrome ownership behavior unchanged: stopping the daemon ends in-memory sessions and leaves Chrome cleanup to existing profile/daemon commands.

The page close policy should preserve the last browser tab rule used by MCP's `close_page`: the last open page remains open.

## Control protocol additions

Possible daemon control commands:

| Command | Response |
|---------|----------|
| `session_page session=<id>` | `session=<id> page=<id> url=<url>` |
| `session_attach session=<id> page=<id>` | `session=<id> page=<id>` |
| `session_new_page session=<id> url=<url> background=<bool>` | `session=<id> page=<id> url=<url>` |
| `session_release_page session=<id>` | `session=<id> page=` |

The existing `session_list` line can append `page=<id>` and `url=<url>` once page ownership exists.

## Implementation phases

### Phase 1: MCP page id routing

Start MCP with `--experimentalPageIdRouting`. Teach the router to rewrite `tools/list` schemas, inject `pageId`, and keep a session page id.

Acceptance checks:

- 2 sessions can bind at the same time.
- Each session can `new_page`, `take_snapshot`, and `click` on its own tab.
- `list_pages` called from each session marks that session's page as the target.
- `select_page` in one session changes that session's target page.

### Phase 2: UID token rewriting

Implemented. Snapshot uids are rewritten to session uid tokens, and uid-bearing request fields are translated before forwarding.

Acceptance checks:

- A uid token from session A is rejected in session B.
- A uid token from an older snapshot epoch is rejected after a newer snapshot replaces it.
- `evaluate_script` element args follow the same mapping as `click` and `fill`.

### Phase 3: lifecycle and fairness

Implemented. Session close and expiry clean up daemon-created pages, attached pages stay open, and daemon status reports active sessions, session pages, and queued MCP requests.

Acceptance checks:

- A daemon-created page is closed when its session expires.
- An attached page stays open when its session expires.
- `daemon status` reports active sessions, page ids, and queued MCP requests.

## Open questions

- Whether `new_page` should always replace a session's page or support multiple pages per session.
- Whether `close_page` should accept an explicit page id from clients or only close the session page.
- Whether `tools/list` schema rewriting should be default or gated behind a daemon capability flag.
- Whether `--experimentalStructuredContent` should be enabled for the daemon so page lists can be parsed from structured data instead of text.
- Whether long-running calls such as `wait_for`, performance tracing, and screencast need per-tool scheduling rules.
- Whether profile-level tools should require an exclusive profile lock mode.

## Operational rule for agents

Each parallel agent owns one session id. The daemon assigns that session to one tab and injects `pageId` for page-scoped tools.

Agents should treat snapshot uid tokens as session-local. A token returned in one session belongs to that session's tab and latest snapshot epoch.
