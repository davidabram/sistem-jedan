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

Each call gets a fresh runtime and context with no filesystem, environment, timers, or module access, and no network access other than `system1.decide()`. Execution is limited by a deadline (enforced with the QuickJS interrupt handler, so synchronous infinite loops are stopped) and a memory limit, 5 seconds and 64 MiB by default; see [Configuration](#configuration).

Failures return a tool error (`isError: true`) with structured content `{ "error": { "kind", "message" } }`, where `kind` is one of `exception`, `timeout`, `out_of_memory`, `never_settled`, `missing_return`, `unsupported_result`, `engine`, or `json`. Invalid arguments produce a plain tool error; unknown tool names produce a JSON-RPC `-32602` error.

## System 1 decisions

When a `[provider]` is configured, JavaScript can ask OpenRouter's hosted System 1 [Decisions API](https://openrouter.ai/docs/guides/features/decisions) through one native function. `system1.decide({ state, questions })` returns a promise resolving to the validated response `{ id, model, provider, answers, usage }`. Questions are `noul` (probability of yes), `choice` or `score`; the endpoint, model and API key always come from the configuration, and unknown input fields are rejected. Calls run concurrently, so one `execute` invocation can fan out and return only the processed result:

```js
const commits = [
  { id: "a", message: "Fix race condition" },
  { id: "b", message: "Change database schema" },
];
const results = await Promise.all(commits.map(async commit => {
  const decision = await system1.decide({
    state: commit,
    questions: { risky: { type: "noul", instructions: "Could this change break production?" } },
  });
  return { id: commit.id, probability: decision.answers.risky.noul };
}));
return results.filter(x => x.probability > 0.7);
```

A failed call rejects its promise with an `Error` named `System1Error` whose `kind` is one of `provider_not_configured`, `call_limit_exceeded`, `invalid_input`, `timeout`, `transport`, `authentication`, `http`, `invalid_response`, `encode`, or `limiter_closed`, so failures can be handled with `try`/`catch` or `Promise.allSettled`. Error messages never contain the API key. A rejection that is never handled fails the execution with kind `exception`.

The execution deadline covers HTTP calls too; when it expires, pending requests are cancelled. At most `max_concurrent_requests` requests are in flight at once across all executions, and each execution may initiate at most `max_calls_per_execution` calls (calls rejected for invalid input do not count).

Stdout carries MCP messages only; diagnostics are logged to stderr. The process exits when the client closes stdin.

## Configuration

Configuration is read from `--config <path>` (or `--config=<path>`), otherwise from `$XDG_CONFIG_HOME/sistem-jedan/config.toml`, falling back to `~/.config/sistem-jedan/config.toml`. An explicit `--config` path must exist; if the default file is absent, the server starts with default execution limits and no provider.

```toml
[provider]
endpoint = "https://openrouter.ai/api/alpha/decisions"
model = "typesafe/jev-1.14"
api_key = "<your OpenRouter API key>"

[execution]
timeout_seconds = 5
memory_limit_mb = 64
max_concurrent_requests = 8
max_calls_per_execution = 32
```

`[provider]` describes OpenRouter's hosted System 1 [Decisions API](https://openrouter.ai/docs/guides/features/decisions) and is required in a configuration file. `endpoint` must be an `https` URL with a host and without embedded credentials, a query string or a fragment. `api_key` holds the API key; since the file contains a secret, restrict it with `chmod 600`. `[execution]` and each of its fields are optional and default to the values shown; `timeout_seconds` must be between 1 and 3600, and every other value must be at least 1. Unknown keys are rejected.

The configuration is validated at startup; any problem is logged and the process exits with an error. The API key is never logged, never returned in MCP responses, and never made visible to JavaScript. Without a provider, ordinary JavaScript still runs and `system1.decide()` rejects with `provider_not_configured`.

To install the configuration, copy [`config.example.toml`](config.example.toml) to the XDG path with owner-only permissions, then replace the placeholder API key:

```sh
install -d -m 700 "${XDG_CONFIG_HOME:-$HOME/.config}/sistem-jedan"
install -m 600 config.example.toml "${XDG_CONFIG_HOME:-$HOME/.config}/sistem-jedan/config.toml"
"${EDITOR:-vi}" "${XDG_CONFIG_HOME:-$HOME/.config}/sistem-jedan/config.toml"
```

A repository-local `config.toml` (for use with `--config config.toml`) is ignored by Git.

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
