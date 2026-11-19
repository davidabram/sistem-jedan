# sistem jedan

Minimal MCP server over stdio, built on the official `rmcp` Rust SDK.

It exposes exactly one tool, `execute`, taking `{ "code": string }`. JavaScript evaluation is not implemented: the code is never run and every valid call returns:

```json
{ "status": "not_implemented" }
```

Invalid arguments produce a tool error (`isError: true`); unknown tool names produce a JSON-RPC `-32602` error.

Stdout carries MCP messages only; diagnostics are logged to stderr. The process exits when the client closes stdin.

## Claude Code integration

Build the binary, then register it with its absolute path:

```sh
nix build                      # binary at ./result/bin/sistem-jedan
claude mcp add --scope user --transport stdio sistem-jedan \
  -- "$(readlink -f result)/bin/sistem-jedan"
```

Options (`--scope`, `--transport`) must come before the server name; everything after `--` is the command Claude Code runs. Check the registration with `claude mcp get sistem-jedan`, or `/mcp` inside a Claude Code session. Remove it with `claude mcp remove --scope user sistem-jedan`.

## Development

Requires Nix with flakes enabled.

```sh
nix develop            # or `direnv allow`; shell with rustc, cargo, rustfmt, clippy, rust-analyzer
nix build              # build the binary into ./result
nix flake check        # build and run tests
nix fmt flake.nix      # format Nix files with nixfmt
```

Supported systems: `x86_64-linux`, `aarch64-linux`, `aarch64-darwin`.
