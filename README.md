# chrome-devtools

`chrome-devtools` is a profile-aware command-line wrapper for running Chrome DevTools MCP operations with isolated Chrome user data directories.

The tool is designed to be called as a regular CLI from agent skills. It is not registered in Hermes as an MCP server.

## Design

- Profiles are explicit: every operation that targets a browser profile requires `--profile <name>`.
- Profiles own their Chrome user data directory and DevTools port.
- Profiles are read from `~/.config/chrome-devtools/config.toml`.
- If the config file is missing on startup, the CLI creates a `default` profile using `~/.config/chrome-devtools/profiles/default`.
- Prefer `user_data_dir` values under `~/.config/chrome-devtools/profiles/<profile-name>` so Chrome profile data stays with the tool config.
- The CLI may execute `chrome-devtools-mcp` internally for a selected profile.
- MCP input and output are passed through without reimplementing individual Chrome DevTools tools.
- Hermes-side Chrome DevTools MCP registration is not required for this workflow.

## Configuration

```toml
[[profiles]]
name = "default"
port = 9222
user_data_dir = "~/.config/chrome-devtools/profiles/default"
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

`mcp help` prints MCP-specific usage, examples, and notes about stdio JSON-RPC forwarding.

## Development

This repository intentionally provides Rust through the Nix flake development shell. Do not assume `cargo` is installed globally.

```sh
nix develop -c cargo check
nix develop -c cargo fmt
nix develop -c cargo clippy
```
