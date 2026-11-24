![Sistem Jedan: kode mode jev your agent](assets/hero.png)

# sistem jedan

An MCP server that lets AI coding agents run JavaScript that queries System 1 models through OpenRouter's [Decisions API](https://openrouter.ai/docs/guides/features/decisions).

> **Experimental.** The interface, config format and error kinds may change. The Decisions API is in alpha.

## What is it?

`sistem-jedan` (Croatian for "system one") is a small Rust MCP server with one tool: `execute`. It runs JavaScript in a sandboxed [QuickJS](https://bellard.org/quickjs/) engine, where `system1.decide()` asks a System 1 model for a structured judgment: a probability, a choice or a score.

**Why code mode?** Asking about 50 commits through a plain `decide` tool takes 50 tool calls, and all 50 answers land in the agent's context. With `execute`, the agent writes one script that runs the decisions concurrently, filters them, and returns only the risky ones.

The API key stays in the server's config. Scripts never see it and can reach the network only through `system1.decide()`.

## Quick Start

You need [Nix](https://nixos.org/download/) with flakes (`x86_64-linux`, `aarch64-linux` or `aarch64-darwin`), an [OpenRouter](https://openrouter.ai) API key, and [Claude Code](https://claude.com/claude-code).

**1. Install**

```sh
nix profile add github:davidabram/sistem-jedan   # older Nix: nix profile install
```

**2. Create the config** (owner-only permissions)

```sh
config_dir="${XDG_CONFIG_HOME:-$HOME/.config}/sistem-jedan"
install -d -m 700 "$config_dir"
(umask 077 && curl -fsSL -o "$config_dir/config.toml" \
  https://raw.githubusercontent.com/davidabram/sistem-jedan/main/config.example.toml)
"${EDITOR:-vi}" "$config_dir/config.toml"
```

**3. Set your key and model.** Replace the placeholder `api_key` with your OpenRouter key. `model` can be any Decisions API model:

```toml
[provider]
endpoint = "https://openrouter.ai/api/alpha/decisions"
model = "typesafe/jev-1.14"          # any Decisions API model
api_key = "<your OpenRouter API key>"
```

**4. Register with Claude Code**

```sh
claude mcp add --scope user --transport stdio sistem-jedan -- ~/.nix-profile/bin/sistem-jedan
```

**5. Verify**

```sh
claude mcp get sistem-jedan
```

If it fails to connect, run `~/.nix-profile/bin/sistem-jedan` directly. A bad config is logged to stderr and the server exits. A good config leaves it waiting on stdin; press Ctrl-D to stop it.

**6. Try it.** In Claude Code, ask:

> Use the sistem-jedan execute tool to ask system1 whether a commit titled "Drop legacy users table" is risky.

## Your first decision

`execute` takes `{ "code": string }`. The code is the **body of an async function**, so top-level `await` works and you must `return` the result.

```js
const decision = await system1.decide({
  state: { file: "src/checkout.js", diff: "-  total = subtotal + tax;\n+  total = subtotal;" },
  questions: {
    risky: { type: "noul", instructions: "Could this break production?" },
  },
});

const answer = decision.answers.risky;
if (answer.type === "refusal") return { risky: null }; // no answer, not 0
return { risky: answer.noul };
```

```json
{ "result": { "risky": 0.93 } }
```

The probability is model output and varies between runs.

The resolved object contains `answers` (keyed by question name), `model`, `id` and `provider` (either may be `null`), and `usage` (token counts; cost fields may be `null`).

## How code mode works

```text
agent ── execute({ code }) ──▶ QuickJS ── system1.decide() ──▶ Decisions API
agent ◀── { "result": … } ──── QuickJS ◀── validated answers ──┘
```

Between calls it's plain JavaScript. Only the returned value goes back to the agent:

```js
const commits = [
  { sha: "a1f3", message: "Fix typo in README" },
  { sha: "b7c2", message: "Drop legacy users table" },
];

const results = await Promise.all(commits.map(async (commit) => {
  const { answers } = await system1.decide({
    state: commit,
    questions: { risky: { type: "noul", instructions: "Could this break production?" } },
  });
  const a = answers.risky;
  return { sha: commit.sha, risk: a.type === "noul" ? a.noul : null };
}));

return {
  risky: results.filter((r) => r.risk !== null && r.risk > 0.7),
  unanswered: results.filter((r) => r.risk === null).map((r) => r.sha),
};
```

## Decisions API

`system1.decide({ state, questions })`

- `state` is a string, object or array, nested at most 64 levels deep.
- `questions` maps names (1–64 chars: `A-Z a-z 0-9 _ -`) to questions. Answers come back under the same names.

Unknown fields are rejected. Every question accepts optional `instructions`. Instructions and criteria must not be blank.

| `type` | `criteria` | Answer |
| --- | --- | --- |
| `noul` | optional `{ true, false }`; needs `instructions` if omitted | `noul`: probability 0–1 |
| `choice` | required `{ id: "description", … }`, at least 2 | `choice`: an id; optional `probabilities` |
| `score` | required `["lowest", …, "highest"]`, at least 2 | `score`: probability-weighted level from 0 to `length - 1`, often fractional; optional `probabilities` and `legend` |

Non-refusal answers may also include `confidence`. Optional fields are `null` when missing. **Any answer can be `{ type: "refusal" }`, so always check `answer.type`.**

```js
const { answers: a } = await system1.decide({
  state: "The export button does nothing in Safari.",
  questions: {
    bug: { type: "noul", instructions: "Is this a bug?" },
    area: { type: "choice", criteria: { frontend: "Browser UI", backend: "Server or API" } },
    severity: { type: "score", criteria: ["Cosmetic", "Minor", "Major", "Blocking"] },
  },
});
return {
  bug: a.bug.type === "noul" ? a.bug.noul : null,
  area: a.area.type === "choice" ? a.area.choice : null,
  severity: a.severity.type === "score" ? a.severity.score : null,
};
```

Score probability and legend keys use canonical decimal levels such as `"0"` and `"1"`. Sparse maps are allowed. Probability maps must sum to 1 within 0.02. Responses are also checked for internal consistency:

- A `choice` must be at least as probable as every other listed choice, within 0.01. Ties are accepted. If the chosen id is missing from a sparse map, no listed choice may be more probable than the mass left unlisted.
- A `score` must be within `0.005 × (1 + sum of listed levels)` score units of the probability-weighted level of its `probabilities`. If the map sums to less or more than 1, the allowed range widens by the missing or excess mass times the highest level.

Responses that fail validation reject with `invalid_response`. The server never corrects or renormalizes a response.

These tolerances are this server's own policy. OpenRouter doesn't document how providers round probabilities or scores. They are sized so that a provider rounding each probability and the score independently to two decimals still passes. Coarser rounding can be rejected even though the response is otherwise legitimate.

## Configuration

The server looks for config in this order:

1. `--config <path>`, which must exist.
2. `$XDG_CONFIG_HOME/sistem-jedan/config.toml`, falling back to `~/.config/sistem-jedan/config.toml`.
3. If neither exists, the server starts without a provider and `system1.decide()` rejects with `provider_not_configured`.

```toml
[provider]                     # required
endpoint = "https://openrouter.ai/api/alpha/decisions"   # https, no credentials/query/fragment
model = "typesafe/jev-1.14"
api_key = "<your OpenRouter API key>"

[execution]                    # optional; defaults shown
timeout_seconds = 5            # whole execution, including HTTP (1–3600)
memory_limit_mb = 64           # QuickJS backing allocations
# vcpu_per_execution = 0.2      # CPU share per execution (0–1]; unset = no cap
max_concurrent_requests = 8    # in-flight inference requests, shared across executions
max_concurrent_executions = 4   # live QuickJS runtimes, shared across the server
max_calls_per_execution = 32   # decide() calls per execute
max_decision_input_total_mb = 8 # serialized decide() input admitted per execute
```

Unknown keys are rejected. An invalid config is logged to stderr, and the server exits. If your scripts chain several decisions, raise `timeout_seconds`.

**Keep the file at mode `600` and never commit it.** The key is never logged, returned to the agent or exposed to JavaScript.

## Running over HTTP

By default the server speaks MCP over stdio. Pass `--http <address>` to serve [Streamable HTTP](https://modelcontextprotocol.io/specification/2025-11-25/basic/transports#streamable-http) at `/mcp` instead:

```sh
sistem-jedan --http 127.0.0.1:8080
claude mcp add --scope user --transport http sistem-jedan http://127.0.0.1:8080/mcp
```

The server accepts only `Host` headers of `localhost`, `127.0.0.1` and `::1`, which blocks DNS rebinding. To reach it under another name, bind a public address and allow each hostname clients use with `--allowed-host` (repeatable; add `name:port` to allow only that port):

```sh
sistem-jedan --http 0.0.0.0:8080 --allowed-host mcp.example.com
```

Other requests get `403 Forbidden`. Every HTTP session shares one set of execution and request limits. SIGINT or SIGTERM closes open sessions and stops the server.

> **There is no authentication.** Anyone who can reach the port can run scripts and spend your OpenRouter credit. Keep it on loopback or a private network, or put it behind a reverse proxy that authenticates and terminates TLS.

## Limits and sandbox

Every admitted `execute` call gets a fresh QuickJS runtime. Scripts have no filesystem, environment variables, modules, timers or network access other than `system1.decide()`.

| Limit | Exceeded → |
| --- | --- |
| `timeout_seconds` | `timeout` (also stops infinite loops) |
| `memory_limit_mb` | `out_of_memory` |
| `vcpu_per_execution` | CPU-bound code is paused to stay under the share; it gets less done before `timeout` |
| `max_concurrent_executions` | `execution_capacity` immediately, before runtime creation |
| `max_concurrent_requests` | extra calls queue, and the wait counts toward the deadline |
| `max_calls_per_execution` | `call_limit_exceeded` |
| `max_decision_input_total_mb` | `input_budget_exceeded` |
| 1 MiB of UTF-8 JavaScript source (fixed) | `invalid_arguments` |
| 1 MiB of serialized JSON per `decide()` argument (fixed) | `invalid_input` (uses up neither budget) |
| 1 MiB of serialized UTF-8 JSON in the returned result (fixed) | `result_too_large` (filter or aggregate in JavaScript before returning) |

The memory limit counts live QuickJS backing allocations, including runtime initialization, arenas and allocator headers. It excludes global allocator overhead, any temporary space used internally by the global allocator during reallocation, and separate allocations made by Rust, the transport or the HTTP client. It is not a limit on process memory.

Each `decide()` call has two independent per-execution budgets. Its serialized input is charged to `max_decision_input_total_mb` as soon as it fits the 1 MiB per-decision limit, before it is parsed and validated, and those bytes stay charged for the rest of the execution, whether the call fails validation, fails at the provider, times out or succeeds. One call is charged to `max_calls_per_execution` once the input passes validation, even if the call fails later, so invalid input spends input bytes but no calls. The input budget limits how much serialized JSON an execution can hand to the Rust side, not the Rust heap or process RSS: parsed requests take more memory than their JSON, and requests waiting for an inference slot stay in memory until they run.

Each execution runs on its own thread, so CPU-bound scripts don't delay MCP requests or inference responses. With `vcpu_per_execution`, the server measures each execution's thread CPU time and pauses it whenever it gets ahead of its share, so busy scripts use at most `max_concurrent_executions` × `vcpu_per_execution` CPUs in total. Time spent waiting on `system1.decide()` earns at most 10 ms of extra CPU, and a single native call, such as filling a large typed array, is only paused after it returns. To size a container, give it that much CPU plus some headroom for the server. For memory, start from about 60 MB + `max_concurrent_executions` × (`memory_limit_mb` + a few times `max_decision_input_total_mb`) and add headroom. This is an estimate, not a guaranteed upper bound: allocations outside QuickJS, such as parsed decision requests, HTTP buffers and responses, aren't capped.

Some MCP clients, including Claude Code, run tool calls one at a time, so a single agent rarely reaches `max_concurrent_executions`. Put independent decisions in one script with `Promise.all` to use `max_concurrent_requests`.

## Errors

An execution ends when the returned promise settles, the deadline expires, the runtime runs out of memory, or MCP cancels the request. Every deadline expiration is `timeout`, including when the returned promise stays pending. A backing allocation failure ends the execution even if JavaScript catches the resulting exception. Execution diagnostics are limited to 8 KiB of UTF-8.

- If the code returns nothing, it fails with `missing_return`. If the return value can't be converted to JSON, it fails with `unsupported_result`.
- A rejection that propagates through the returned promise fails with `exception`.
- **Detached promises** (neither awaited nor returned) may never finish, and their rejections are ignored.

A failed `execute` call returns `isError: true` with:

```json
{ "error": { "kind": "timeout", "message": "execution exceeded the 5000ms deadline" } }
```

The possible kinds are `invalid_arguments`, `execution_capacity`, `exception`, `timeout`, `out_of_memory`, `missing_return`, `unsupported_result`, `result_too_large`, `engine` and `json`.

A failed `system1.decide()` call rejects with a `System1Error` that has a `kind` property:

| `kind` | Meaning |
| --- | --- |
| `provider_not_configured` | no config file |
| `invalid_input` | bad, unserializable or oversized input |
| `call_limit_exceeded` | per-execution call budget used up |
| `input_budget_exceeded` | per-execution decision input budget used up |
| `authentication` | HTTP 401/403: check `api_key` |
| `http`, `timeout`, `transport` | other HTTP, timeout or network failures |
| `invalid_response` | response didn't match the questions |

To keep partial results, use `Promise.allSettled`:

```js
const items = ["first", "second", "third"];
const settled = await Promise.allSettled(items.map((item) =>
  system1.decide({ state: item, questions: { relevant: { type: "noul", instructions: "Is this relevant?" } } })
));
return settled.map((s) => s.status === "fulfilled" ? s.value.answers.relevant : { error: s.reason.kind });
```

## Other ways to install

```sh
nix run github:davidabram/sistem-jedan -- --config ./config.toml   # run without installing
nix profile add .                                                  # from a local checkout
nix profile upgrade sistem-jedan                                   # upgrade
```

With NixOS or home-manager, add `inputs.sistem-jedan.url = "github:davidabram/sistem-jedan"` and install `inputs.sistem-jedan.packages.${pkgs.stdenv.hostPlatform.system}.default`. Then register the binary with `"$(command -v sistem-jedan)"`.

Don't register a `/nix/store/...` path from `./result`. It breaks after garbage collection.

## Development

```sh
nix develop        # or `direnv allow`
nix build          # binary in ./result
cargo fmt
cargo clippy       # pedantic lints enabled
nix flake check    # builds the package and runs its cargo tests
nix fmt flake.nix
```

MCP traffic goes over stdio, logs go to stderr, and the server exits when stdin closes. With `--http`, it runs until it gets SIGINT or SIGTERM.
