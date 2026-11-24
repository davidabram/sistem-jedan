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
| `noul` | optional `{ true, false }` | `noul`: probability 0–1 |
| `choice` | required `{ id: "description", … }`, at least 2 | `choice`: an id; optional `probabilities` |
| `score` | required `["lowest", …, "highest"]`, at least 2 | `score`: 0 to `length - 1`; optional `probabilities` and `legend` |

Non-refusal answers may also include `confidence`. Optional fields are `null` when missing. **Any answer can be `{ type: "refusal" }`, so always check `answer.type`.**

```js
const { answers: a } = await system1.decide({
  state: "The export button does nothing in Safari.",
  questions: {
    bug: { type: "noul" },
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

Responses that don't match your questions reject with `invalid_response`.

## Configuration

The server looks for config in this order:

1. `--config <path>`, which must exist. This is the only CLI option.
2. `$XDG_CONFIG_HOME/sistem-jedan/config.toml`, falling back to `~/.config/sistem-jedan/config.toml`.
3. If neither exists, the server starts without a provider and `system1.decide()` rejects with `provider_not_configured`.

```toml
[provider]                     # required
endpoint = "https://openrouter.ai/api/alpha/decisions"   # https, no credentials/query/fragment
model = "typesafe/jev-1.14"
api_key = "<your OpenRouter API key>"

[execution]                    # optional; defaults shown
timeout_seconds = 5            # whole execution, including HTTP (1–3600)
memory_limit_mb = 64           # QuickJS heap
max_concurrent_requests = 8    # in-flight requests, shared across executions
max_calls_per_execution = 32   # decide() calls per execute
```

Unknown keys are rejected. An invalid config is logged to stderr, and the server exits. If your scripts chain several decisions, raise `timeout_seconds`.

**Keep the file at mode `600` and never commit it.** The key is never logged, returned to the agent or exposed to JavaScript.

## Limits and sandbox

Every `execute` call gets a fresh QuickJS runtime. Scripts have no filesystem, environment variables, modules, timers or network access other than `system1.decide()`.

| Limit | Exceeded → |
| --- | --- |
| `timeout_seconds` | `timeout` (also stops infinite loops) |
| `memory_limit_mb` | `out_of_memory` |
| `max_concurrent_requests` | extra calls queue, and the wait counts toward the deadline |
| `max_calls_per_execution` | `call_limit_exceeded` |
| 1 MiB of serialized JSON per `decide()` argument (fixed) | `invalid_input` (doesn't use up the call budget) |

The memory limit covers the JavaScript heap only, not the server's Rust allocations. Calls count toward the budget once their input passes validation, even if they fail later.

## Errors

An execution ends when the returned promise settles.

- If the code returns nothing, it fails with `missing_return`. If the return value can't be converted to JSON, it fails with `unsupported_result`.
- A rejection that propagates through the returned promise fails with `exception`.
- **Detached promises** (neither awaited nor returned) may never finish, and their rejections are ignored.

A failed `execute` call returns `isError: true` with:

```json
{ "error": { "kind": "timeout", "message": "execution exceeded the 5000ms deadline" } }
```

The possible kinds are `invalid_arguments`, `exception`, `timeout`, `out_of_memory`, `never_settled`, `missing_return`, `unsupported_result`, `engine` and `json`.

A failed `system1.decide()` call rejects with a `System1Error` that has a `kind` property:

| `kind` | Meaning |
| --- | --- |
| `provider_not_configured` | no config file |
| `invalid_input` | bad, unserializable or oversized input |
| `call_limit_exceeded` | per-execution budget used up |
| `authentication` | HTTP 401/403: check `api_key` |
| `http`, `timeout`, `transport` | other HTTP, timeout or network failures |
| `invalid_response` | response didn't match the questions |

To keep partial results, use `Promise.allSettled`:

```js
const items = ["first", "second", "third"];
const settled = await Promise.allSettled(items.map((item) =>
  system1.decide({ state: item, questions: { relevant: { type: "noul" } } })
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

MCP traffic goes over stdio, logs go to stderr, and the server exits when stdin closes.
