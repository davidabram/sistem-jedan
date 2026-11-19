# sistem jedan

Minimal MCP server over stdio, built on the official `rmcp` Rust SDK.

It exposes exactly one tool, `execute`, taking `{ "code": string }`. The code runs in a sandboxed [QuickJS](https://bellard.org/quickjs/) engine (via `rquickjs`) as the body of an async function, so `await` works at the top level and the result is whatever the code explicitly `return`s:

```js
const values = await Promise.all([Promise.resolve(10), Promise.resolve(20)]);
return { sum: values.reduce((a, b) => a + b, 0) };
```

```json
{ "result": { "sum": 30 } }
```

Each call gets a fresh runtime and context with no filesystem, network, environment, timers, or module access. Execution is limited to 5 seconds (enforced with the QuickJS interrupt handler, so synchronous infinite loops are stopped) and 64 MiB of memory.

Failures return a tool error (`isError: true`) with structured content `{ "error": { "kind", "message" } }`, where `kind` is one of `exception`, `timeout`, `out_of_memory`, `never_settled`, `missing_return`, `unsupported_result`, `engine`, or `json`. Invalid arguments produce a plain tool error; unknown tool names produce a JSON-RPC `-32602` error.

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
