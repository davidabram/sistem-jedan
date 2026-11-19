![Sistem Jedan: kode mode jev your agent](assets/hero.png)

# sistem jedan

> **Experimental.** This project is early and unstable. The tool interface, configuration format and error kinds may change without notice, and the Decisions API it targets is itself an alpha endpoint. Don't rely on it for anything important yet.

`sistem-jedan` is a minimal MCP server over stdio, built on the official `rmcp` Rust SDK. It gives an MCP client (such as Claude Code) a **code mode** for working with **System 1 models**: instead of making one tool call per decision, the agent writes a short JavaScript program that calls the model as many times as it needs, processes the answers, and returns only the result.

It works with any System 1 model served through OpenRouter's [Decisions API](https://openrouter.ai/docs/guides/features/decisions). The model is just a string in the configuration, so switching models means changing one line.

## Code mode

The server exposes exactly one tool, `execute`, taking `{ "code": string }`. The code runs in a sandboxed [QuickJS](https://bellard.org/quickjs/) engine (embedded via `rquickjs`) as the body of an async function, so `await` works at the top level and the result is whatever the code explicitly `return`s:

```js
const values = await Promise.all([Promise.resolve(10), Promise.resolve(20)]);
return { sum: values.reduce((a, b) => a + b, 0) };
```

```json
{ "result": { "sum": 30 } }
```

Why code instead of plain tool calls: when an agent has to ask a model about 50 items, 50 separate tool calls fill its context with intermediate results. In code mode it loops, fans out with `Promise.all`, filters and aggregates inside the sandbox, and only the final value comes back.

### Sandbox

Each call gets a fresh QuickJS runtime and context with no filesystem, environment, timers, or module access, and no network access other than `system1.decide()`. Execution is limited by a deadline (enforced with the QuickJS interrupt handler, so synchronous infinite loops are stopped) and a memory limit, 5 seconds and 64 MiB by default; see [Configuration](#configuration).

Failures return a tool error (`isError: true`) with structured content `{ "error": { "kind", "message" } }`, where `kind` is one of `exception`, `timeout`, `out_of_memory`, `never_settled`, `missing_return`, `unsupported_result`, `engine`, or `json`. Invalid arguments produce a plain tool error; unknown tool names produce a JSON-RPC `-32602` error.

## System 1 decisions

When a `[provider]` is configured, JavaScript can call the configured System 1 model through one native function. `system1.decide({ state, questions })` returns a promise resolving to the validated response `{ id, model, provider, answers, usage }`. Questions are `noul` (probability of yes), `choice` or `score`; the endpoint, model and API key always come from the configuration, and unknown input fields are rejected. Calls run concurrently, so one `execute` invocation can fan out and return only the processed result:

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

## Installation with Nix

The repository is a Nix flake exposing `packages.default` for `x86_64-linux`, `aarch64-linux` and `aarch64-darwin`. Flakes must be enabled (`experimental-features = nix-command flakes`).

**Install into your user profile** (recommended):

```sh
nix profile install github:davidabram/sistem-jedan
# or, from a local checkout:
nix profile install .
```

The binary is then available at `~/.nix-profile/bin/sistem-jedan`, a path that stays valid across upgrades (`nix profile upgrade sistem-jedan`) and garbage collection.

**Try it without installing:**

```sh
nix run github:davidabram/sistem-jedan -- --config ./config.toml
```

**Declaratively** with NixOS or home-manager, add the flake as an input and reference its package:

```nix
{
  inputs.sistem-jedan.url = "github:davidabram/sistem-jedan";

  # in a home-manager module:
  home.packages = [
    inputs.sistem-jedan.packages.${pkgs.stdenv.hostPlatform.system}.default
  ];
  # or in a NixOS module: environment.systemPackages = [ ... ];
}
```

Avoid registering a `/nix/store/...` path taken from `./result`: once that build is replaced and garbage-collected, the path disappears and the MCP registration breaks.

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

`[provider]` selects the System 1 model and is required in a configuration file. `model` can be any model the Decisions API serves; `typesafe/jev-1.14` is only an example. `endpoint` must be an `https` URL with a host and without embedded credentials, a query string or a fragment. `api_key` holds your OpenRouter API key; since the file contains a secret, restrict it with `chmod 600`. `[execution]` and each of its fields are optional and default to the values shown; `timeout_seconds` must be between 1 and 3600, and every other value must be at least 1. Unknown keys are rejected.

The configuration is validated at startup; any problem is logged and the process exits with an error. The API key is never logged, never returned in MCP responses, and never made visible to JavaScript. Without a provider, ordinary JavaScript still runs and `system1.decide()` rejects with `provider_not_configured`.

To install the configuration, copy [`config.example.toml`](config.example.toml) to the XDG path with owner-only permissions, then replace the placeholder API key:

```sh
install -d -m 700 "${XDG_CONFIG_HOME:-$HOME/.config}/sistem-jedan"
(umask 077 && curl -fsSL -o "${XDG_CONFIG_HOME:-$HOME/.config}/sistem-jedan/config.toml" \
  https://raw.githubusercontent.com/davidabram/sistem-jedan/main/config.example.toml)
"${EDITOR:-vi}" "${XDG_CONFIG_HOME:-$HOME/.config}/sistem-jedan/config.toml"
```

From a local checkout, use `install -m 600 config.example.toml <path>` instead of `curl`. A repository-local `config.toml` (for use with `--config config.toml`) is ignored by Git.

## Claude Code integration

After installing with `nix profile install`, register the server:

```sh
claude mcp add --scope user --transport stdio sistem-jedan -- ~/.nix-profile/bin/sistem-jedan
```

If you installed declaratively, use the binary's path on your `PATH` instead (for example `"$(command -v sistem-jedan)"`).

Options (`--scope`, `--transport`) must come before the server name; everything after `--` is the command Claude Code runs. Check the registration with `claude mcp get sistem-jedan`, or `/mcp` inside a Claude Code session. Remove it with `claude mcp remove --scope user sistem-jedan`.

## Development

```sh
nix develop            # or `direnv allow`; shell with rustc, cargo, rustfmt, clippy, rust-analyzer
nix build              # build the binary into ./result
nix flake check        # build and run tests
nix fmt flake.nix      # format Nix files with nixfmt
```
