# chrome-devtools

`chrome-devtools` is a profile-aware command-line wrapper for running Chrome DevTools MCP operations with isolated Chrome user data directories.

The tool is designed to be called as a regular CLI from agent skills. It is not registered in Hermes as an MCP server.

## Design

- Profiles are explicit: every operation that targets a browser profile requires `--profile <name>`.
- Profiles own their Chrome user data directory and DevTools port.
- The CLI may execute `chrome-devtools-mcp` internally for a selected profile.
- MCP input and output are passed through without reimplementing individual Chrome DevTools tools.
- Hermes-side Chrome DevTools MCP registration is not required for this workflow.

## Commands

```sh
chrome-devtools exec --profile sana-twitter
chrome-devtools list
chrome-devtools status --profile sana-twitter
chrome-devtools stop --profile sana-twitter
```

`exec` starts or reuses the Chrome instance for the selected profile, then runs `chrome-devtools-mcp` with that profile's DevTools URL. Standard input, output, and error are inherited so MCP messages pass through the upstream process.

## Development

This repository intentionally provides Rust through the Nix flake development shell. Do not assume `cargo` is installed globally.

```sh
nix develop -c cargo check
nix develop -c cargo fmt
nix develop -c cargo clippy
```
