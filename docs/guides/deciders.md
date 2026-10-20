# Decision models

A *decision model* answers a typed question about some text or JSON: is this true, which
of these options, where on this scale. It doesn't write text. It returns calibrated
probabilities in one pass, for a fraction of an LLM's cost and latency. TypeSafe's Jev
was the first with a public API. OpenAI's Decisions API, Cloudflare's Clef, and the
open-weight Kev (which runs on Ollama or Ollaya) followed.

hlock uses decision models in three places:

- **In your agent.** A decider can answer your harness's permission prompts (`approve`),
  keep it working when it stops before the task is done (`stop`), narrow the tools it's
  offered (`tool`), or pick which model takes a step (`route`). hlock locks the
  decider in `harness.lock` and wires it into the harness with a hook.
- **As the judge** of an [eval pack](eval-packs.md#judges): it answers each criterion as a
  yes/no question, cheaply enough to run on every trial.
- **For hlock's own checks**: drafting eval packs [from traces](eval-packs.md#draft-a-pack-from-your-traces),
  and the [meta harness](meta.md#decision-models)'s checks of its jobs and its built-in agent.

## Name a decision model

A decider is `provider:model`. The provider decides how hlock reaches it:

| Reference | Protocol | Where | API key |
|---|---|---|---|
| `typesafe:jev-1.13.0` | System One | `https://api.typesafe.ai` (`TYPESAFE_BASE_URL`) | `TYPESAFE_API_KEY` |
| `openai-decisions:gpt-6-luna` | Decisions | `https://api.openai.com/v1` | `OPENAI_API_KEY` |
| `ollama:kev:4b` | System One | a local Ollama 0.40.4 or later (`OLLAMA_HOST`) | none |
| `ollaya:kev:4b` | System One | a local [Ollaya](https://ollaya.dev) (`OLLAYA_HOST`) | `OLLAYA_API_KEY`, if set |
| `openrouter:typesafe/jev-1.14` | System One | `https://openrouter.ai/api` | `OPENROUTER_API_KEY` |
| `cloudflare:clef` | System One | `base-url`, with your account | `CLOUDFLARE_API_TOKEN` |
| `system-one:<model>` | System One | `base-url` (Kev's own server, SGLang, vLLM, LiteLLM) | `key-env`, if any |

Use a version (`jev-1.13.0`), not an alias (`jev-latest`): the lock records the model
that answered either way, but an alias can change between runs.

## Try one

`hlock decide ask` puts questions to a decision model about some text, a file (`@path`),
or stdin (`-`):

```bash
hlock decide ask @ticket.txt --noul "Is it urgent?" --choice "team=billing,auth,infra" \
    --score "severity=cosmetic|broken|outage" --model ollama:kev:4b
```

It prints each answer with its probability, the model that answered, the input tokens,
and the cost where the provider publishes a price. Without `--model` it uses
`[deciders.default]` in `harness.toml`, else a Kev model a local Ollama or Ollaya is
serving, else Jev when `TYPESAFE_API_KEY` is set. `--json` prints everything.

## Add deciders to your agent

List them in `harness.toml`, by what they decide:

```toml title="harness.toml"
[deciders.approve]                 # the harness's permission prompts
model = "typesafe:jev-1.13.0"
policy = ".hlock/approve.md"       # your rules, in your own words

[deciders.stop]                    # keep going when the task isn't done
model = "ollama:kev:4b"
```

| Key | Description |
|---|---|
| `model` | The decider, or `none` |
| `any-of` | Candidates for [`hlock resolve`](resolve.md) to choose between, instead of `model`. `none` is one of them. |
| `may` | `approve` only: which of `allow`, `ask` and `deny` it may answer. Default `["deny", "ask"]`, so it can only make the agent more careful. Add `allow` to let it approve calls for you. |
| `policy` | A file of rules added to the question. `hlock check` reports when it changes. |
| `min-probability` | How sure it must be to act: a number, for `approve` a table (`{ deny = 0.8 }`), or a list for `resolve` to choose from. Defaults: allow 0.9, ask 0.5, deny 0.8, stop 0.9, route 0.6; `tool` drops a tool under 0.3. |
| `hooks` | The harness events to wire it to, instead of the profile's default |
| `timeout-ms` | How long the hook waits for an answer (default 2000) |
| `base-url`, `key-env`, `protocol` | Override the provider's defaults |

A decider never blocks your agent. When it's unsure, slow, can't be reached, or answers
something `may` doesn't allow, the hook prints nothing and the harness does what it would
have done.

`hlock pin` locks the deciders with the rest of your setup (here with `--user`, since the
settings live in `~/.claude`):

```console
$ hlock pin --user --harness claude
✓ pinned claude-code 2.1.0 → harness.lock  (--harness)
╭─ 📌 harness.lock · pinned ────────────────────────────────────────────────────────╮
│                                                                                   │
│  harness            claude-code 2.1.0                                             │
│  scope              project + user                                                │
│  llm · default      anthropic:opus                                                │
│  config             {root}/settings.json                                          │
│  decider · approve  typesafe:jev-1.13.0  hook: PermissionRequest · may deny, ask  │
│  decider · stop     ollama:kev:4b  hook: Stop                                     │
│                                                                                   │
╰───────────────────────────────────────────────────────────────────────────────────╯
vendored 1 files into .hlock/vendor/claude-code/  · review before committing
next → hlock check  ·  hlock sync
```

and `hlock sync` wires them in, and says what each still needs:

```console
$ hlock sync --yes
Sync plan for Claude Code 2.1.0 → .hlock/env/claude-code/config
✓  harness  Claude Code 2.1.0
●  config   Write .hlock/env/claude-code/config/settings.json
●  decider  Wire deciders (approve, stop) into .hlock/env/claude-code/config/settings.json
○  decider  TYPESAFE_API_KEY for the approve (typesafe:jev-1.13.0) decider  not set; the
            hooks defer to the harness until it is
○  model    kev:4b on ollama (the stop decider)  not served by a running ollama
✓ Write .hlock/env/claude-code/config/settings.json
✓ Wire deciders (approve, stop) into .hlock/env/claude-code/config/settings.json

2 steps need you:
  1. TYPESAFE_API_KEY for the approve (typesafe:jev-1.13.0) decider  not set; the hooks
defer to the harness until it is
     export TYPESAFE_API_KEY=…   # then re-run hlock sync --prefix .hlock/env
  2. kev:4b on ollama (the stop decider)  not served by a running ollama
     ollama pull kev:4b
next → hlock sync --skill (let your agent do it)
```

`hlock sync --pull` pulls a local decider's model through its runtime. The hooks land in
the harness's own config, next to any hooks of yours, which hlock leaves alone:

```json title=".hlock/env/claude-code/config/settings.json"
{
  "hooks": {
    "PermissionRequest": [
      {"matcher": "*", "hooks": [{"type": "command", "timeout": 3,
        "command": "hlock decide hook approve --format claude-code --lock /path/to/project/harness.lock"}]}
    ],
    "Stop": [
      {"hooks": [{"type": "command", "timeout": 3,
        "command": "hlock decide hook stop --format claude-code --lock /path/to/project/harness.lock"}]}
    ]
  },
  "model": "opus"
}
```

`hlock pin` leaves hlock's own hooks out of what it vendors, so a synced config isn't
drift. A decider you remove from `harness.toml` loses its hook at the next sync.

### What each use does

- **`approve`** gets the tool call (its name, input and directory) and your policy, and
  picks `allow`, `ask` or `deny`. In Claude Code and Codex it's wired to the
  `PermissionRequest` event by default: it only sees calls you'd be asked about anyway, so
  it adds no delay to the rest. There it can deny a call, or approve it when `may` has
  `allow`. Set `hooks = ["PreToolUse"]` to have it see every call; it can then also ask
  you about a call that would otherwise run (Claude Code only).
- **`stop`** gets your first request and the agent's last message when the agent stops.
  If it isn't sure the request is done, the agent gets one more turn with a reason. It
  never fires twice in a row, so it can't loop.
- **`tool`** gets the tools the agent's next request offers and the recent messages, and
  drops the ones the next step clearly doesn't need (P(needed) under 0.3). When it would
  keep all of them or none, it does nothing.
- **`route`** picks which of the agent's model roles takes the next step. Only code
  harnesses ask it (below).

### Which harnesses

| Harness | How | Uses |
|---|---|---|
| Claude Code | hooks in `settings.json` | `approve`, `stop` |
| Codex | hooks in `config.toml`'s `[hooks]` | `approve`, `stop` (its `PreToolUse` can't ask) |
| Gemini CLI | hooks in `settings.json`: `BeforeTool`, `AfterAgent`, `BeforeToolSelection` | `approve` (allow or deny), `stop`, `tool` |
| Cursor | hooks in `hooks.json`: `preToolUse` | `approve` |
| Copilot CLI | `.github/hooks/hlock.json` in the repository, the only place Copilot reads hooks | `approve` (deny only) |
| Amp | a plugin hlock writes: `plugins/hlock-deciders.js` | `approve` (allow or refuse), `stop` |
| OpenCode | a plugin hlock writes: `plugins/hlock-deciders.js` | `approve` (refuse only) |
| Pi | an extension hlock writes: `extensions/hlock-deciders.ts` | `approve` (refuse only) |
| Hermes | its own `auxiliary.approval` setting, recorded by `hlock pin` | `approve` (read, not written) |
| Code harnesses | environment variables, and `ctx.decide` (below) | any |
| `reference` | simulated by `hlock resolve` | any |

`pin` and `resolve` say so when a `[deciders.*]` use can't be wired into the harness.
Goose has no hook a decider can answer yet. Pi's own classifier models are built-in
providers it reaches with your keys, not configuration, so `pin` has nothing of them to
record.

The plugins for Amp, OpenCode and Pi are small files that run `hlock decide hook` and act
on its answer; they do nothing when hlock isn't there or isn't sure. They're tested by
loading them with a stand-in for each harness's plugin API, not inside the harnesses
themselves. Cursor's docs don't say whether `cursor-agent` runs hooks.

### Code harnesses

A code harness gets `HLOCK_DECIDER_<USE>` (the reference), `HLOCK_DECIDER_<USE>_BASE_URL`
and `HLOCK_DECIDER_<USE>_PROTOCOL` for each locked decider. For PydanticAI it also sets
`SYSTEM_ONE_BASE_URL` and `SYSTEM_ONE_API_KEY`, and for LangChain-based harnesses and
Google ADK `TYPESAFE_BASE_URL` and `TYPESAFE_API_KEY`, so their own decision-model support
uses the locked one with no code.

To ask hlock's questions instead, a Python entrypoint calls `ctx.decide`:

```python title="my_agent.py"
def run(prompt, ctx):
    pick = ctx.decide("route", options=["planner", "executor"], state={"step": prompt})
    role = pick.get("answer", "planner")      # {} when there's no decider or it isn't sure
    keep = ctx.decide("tool", tools=["search", "edit", "deploy"], state={"step": prompt})
    ...
```

`ctx.decide("approve", tool=…, input=…)` and `ctx.decide("stop", request=…, last_message=…)`
work the same way. Other code harnesses run `$HLOCK_BIN decide hook <use> --format plugin
--lock "$HLOCK_DECIDE_LOCK" --input '<JSON>'` themselves. During `hlock resolve`,
`HLOCK_DECIDE_LOCK` holds just the trial's deciders.

## Check and change them

`hlock check` reports a decider that changed in `harness.toml`, a changed policy file,
and a newer hlock whose questions for a use differ from the ones locked (`hlock sync
--plan` shows whether the hooks are in place):

```console
$ hlock check
✗ drift: your claude-code setup changed since harness.lock was pinned
  decider approve: policy .hlock/approve.md changed since the lock was written
  hint: re-pin with hlock pin --user, or restore it with hlock sync --in-place
```

With `any-of`, [`hlock resolve`](resolve.md) measures each candidate on your evals and
locks the winner. A list of `min-probability` values is searched the same way, so resolve
can pick how sure a decider must be:

```toml title="harness.toml"
[deciders.stop]
any-of = ["ollama:kev:4b", "ollama:kev:9b"]
min-probability = [0.7, 0.9]       # four candidates: each model at each threshold
```

Outside `resolve`, `pin` locks the first value of the list. The offline `reference` harness simulates deciders, so it can't show
what one adds; resolve against your own harness for that.
[`hlock localize`](localize.md) moves hosted deciders to Kev on the local runtime (the 9B
with 16 GB or more to spare, else the 4B), and notes that their scores may change.
[`hlock upgrade`](upgrade.md) lists a newer version of a locked decider
(`typesafe:jev-1.13.0` → `typesafe:jev-1.14.0`), and with `--apply` re-resolves with it,
behind the same evidence gate as model upgrades.

## How they're locked

```toml title="harness.lock"
[[decider]]
use = "approve"
provider = "typesafe"
model = "jev-1.13.0"
protocol = "systemone"
endpoint = "https://api.typesafe.ai"
key-env = "TYPESAFE_API_KEY"
resolved-from = "typesafe:jev-1.13.0"
via = "hook"
may = ["deny", "ask"]
questions = "sha256:…"         # hlock's questions for this use

[[decider.wiring]]
file = "settings"
event = "PermissionRequest"

[decider.policy]
path = ".hlock/approve.md"
digest = "sha256:…"
```

See [the lockfile reference](../reference/lockfile.md#decider) for every key.
