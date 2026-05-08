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
- MCP input and output are passed through without reimplementing individual Chrome DevTools tools.
- Hermes-side Chrome DevTools MCP registration is not required for this workflow.
- Snapshot `uid` values are MCP-process-local. Keep `take_snapshot -> click/fill` in the same `mcp call` stream.
- `mcp call` and `mcp list` take a per-profile lock under `~/.cache/chrome-devtools/locks` so competing agents do not interleave MCP processes for the same Chrome profile.
- The planned daemon direction is one long-lived broker per profile, so multiple agents do not create competing MCP processes for the same Chrome profile.

## Configuration

```toml
[[profiles]]
name = "default"
port = 9222
```

## Commands

```sh
chrome-devtools mcp list --profile default
chrome-devtools mcp call --profile default
chrome-devtools mcp help
chrome-devtools profile list
chrome-devtools profile status --profile default
chrome-devtools profile stop --profile default
```

`mcp list` starts or reuses the Chrome instance for the selected profile, queries `tools/list`, and prints the raw MCP JSON response.

`mcp call` starts or reuses the Chrome instance for the selected profile, then runs `chrome-devtools-mcp` with that profile's DevTools URL. Standard input, output, and error are inherited so MCP messages pass through the upstream process.

Important: `take_snapshot` result `uid` values are local to the running MCP process. Do not split `take_snapshot` and later `click`/`fill` calls across separate `chrome-devtools mcp call` invocations. Keep the MCP process alive for the whole interaction sequence.

`mcp call` and `mcp list` take a per-profile lock under `~/.cache/chrome-devtools/locks`. This is a conservative guardrail until the daemon exists: one profile gets one MCP process at a time, which avoids snapshot-cache and active-tab races. The default wait is 300 seconds and can be changed with `CHROME_DEVTOOLS_LOCK_TIMEOUT_SECS`.

See [`docs/session-daemon-design.md`](docs/session-daemon-design.md) for the planned per-profile daemon/broker direction.

`mcp help` prints MCP-specific usage, examples, and notes about stdio JSON-RPC forwarding.

## Development

This repository intentionally provides Rust through the Nix flake development shell. Do not assume `cargo` is installed globally.

```sh
nix develop -c cargo check
nix develop -c cargo fmt
nix develop -c cargo clippy
```
