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

Each call gets a fresh runtime and context with no filesystem, network, environment, timers, or module access. Execution is limited by a deadline (enforced with the QuickJS interrupt handler, so synchronous infinite loops are stopped) and a memory limit, 5 seconds and 64 MiB by default; see [Configuration](#configuration).

Failures return a tool error (`isError: true`) with structured content `{ "error": { "kind", "message" } }`, where `kind` is one of `exception`, `timeout`, `out_of_memory`, `never_settled`, `missing_return`, `unsupported_result`, `engine`, or `json`. Invalid arguments produce a plain tool error; unknown tool names produce a JSON-RPC `-32602` error.

Stdout carries MCP messages only; diagnostics are logged to stderr. The process exits when the client closes stdin.

## Configuration

Configuration is read from `--config <path>` (or `--config=<path>`), otherwise from `$XDG_CONFIG_HOME/sistem-jedan/config.toml`, falling back to `~/.config/sistem-jedan/config.toml`. An explicit `--config` path must exist; if the default file is absent, the server starts with default execution limits and no provider.

```toml
[provider]
endpoint = "https://provider.example/v1/decisions"
model = "configured-model"
api_key_env = "SYSTEM1_API_KEY"

[execution]
timeout_seconds = 5
memory_limit_mb = 64
max_concurrent_requests = 8
max_calls_per_execution = 32
```

`[provider]` describes the hosted System 1 Decisions API and is required in a configuration file. `endpoint` must be an `https` URL without embedded credentials. Set exactly one of `api_key` (inline) or `api_key_env` (the name of an environment variable holding the key). `[execution]` and each of its fields are optional and default to the values shown; `timeout_seconds` must be between 1 and 3600, and every other value must be at least 1. Unknown keys are rejected.

The configuration is validated and the credential is resolved at startup; any problem is logged and the process exits with an error. The credential is never logged, never returned in MCP responses, and never made visible to JavaScript. Hosted inference itself is not implemented yet, so `max_concurrent_requests` and `max_calls_per_execution` are validated but not yet enforced.

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
