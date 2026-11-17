# Lint instructions for a model

Instructions pile up. A line that helped one model can cost you on the next: newer models
follow instructions more literally, so "answer directly without deliberating" or "never
retry" is followed exactly, or is ignored and only costs tokens. `hlock lint` finds those
lines in what a lock places (skills, instruction files such as `CLAUDE.md` and `AGENTS.md`,
subagents, slash commands) and in any file you name, for the model you're moving to.

The rules are deterministic: phrase patterns, no model calls. Each rule names the models it
applies to, with a link to the advice behind it. The first set comes from Anthropic's
[Prompting Claude Fable 5.2](https://platform.claude.com/docs/en/build-with-claude/prompt-engineering/prompting-claude-fable-5-2)
guide, which lists the harness text to remove or rewrite when you migrate.

Lint only reads. To try deleting what it flags, measured on your tasks, use the
[`ablate`](tune.md#delete-first-ablate) optimizer, or
[`hlock upgrade --to MODEL --retune`](upgrade.md#move-to-a-new-model), which lints, sweeps
the effort, and runs ablate in one step.

## Run it

```console
$ hlock lint --file CLAUDE.md --model claude-fable-5-2
Linting for claude-fable-5-2  10 rules, 1 file
CLAUDE.md
     3:12  warning token-budget-in-prompt  budget of 200k tokens
     4:22  warning think-less              answer directly without deliberating
     8:3   warning stale-caution           Never retry a failed deploy.

token-budget-in-prompt: A large token budget shown in the prompt changes how much the model spends; removing it lowered cost with equal performance.
  → Remove the budget from the prompt text and test your tasks without it (the API's task budgets are a separate feature).  https://platform.claude.com/docs/en/build-with-claude/prompt-engineering/prompting-claude-fable-5-2#review-instructions-written-for-earlier-models
think-less: An instruction to think less doesn't stop the model thinking at medium effort and above.
  → Remove the line and lower the effort level for these turns instead.  https://platform.claude.com/docs/en/build-with-claude/prompt-engineering/prompting-claude-fable-5-2#calibrate-effort
stale-caution: An old caution like this keeps being followed after its reason is gone, and can block autonomous work.
  → Delete it if its reason no longer holds, or narrow it to the case it was written for.  https://platform.claude.com/docs/en/build-with-claude/prompt-engineering/prompting-claude-fable-5-2#keep-caution-from-piling-up
! 3 warnings, 2 info  (-v lists them)
```

That `CLAUDE.md` was:

```markdown title="CLAUDE.md"
# Working here

You have a budget of 200k tokens for each task.
For quick questions, answer directly without deliberating.

- Always delegate codebase searches to subagents.
- Run the tests before you push.
- Never retry a failed deploy.
- Never edit the generated files.
```

`-v` also lists the two info findings: the standing order to delegate, and "Never edit the
generated files.", a rule with no scope or reason.

Without `--file`, `hlock lint` reads what `harness.lock` places (or the lock you name, as
`hlock audit LOCK` takes it). Without `--model`, it lints for the lock's default model.
When there's no model to go by, only the rules for every model apply.

## The rules

`hlock lint --rules --model MODEL` lists the rules that apply to a model; `--model all`
applies every rule.

| Rule | Severity | Models | Finds |
|---|---|---|---|
| `think-less` | warning | Fable 5.2, Mythos 5.2 | "answer directly without deliberating", "don't overthink": these don't stop the model thinking at medium effort and above; lower the effort instead |
| `visible-reasoning` | warning | Fable 5.2, Mythos 5.2 | "think step by step", "write out your full reasoning", `<scratchpad>` tags: reasoning asked for in the response can be refused as reasoning extraction |
| `token-budget-in-prompt` | warning | Fable 5.2, Mythos 5.2 | "you have a budget of 200k tokens" in the prompt text |
| `treat-answer-as-done` | warning | Fable 5.2, Mythos 5.2 | "treat that answer as done", "don't revisit earlier answers": the model stops pointing out its own mistakes |
| `stale-caution` | warning | Fable 5.2, Mythos 5.2 | a bare "never retry", "stop at the first error" |
| `standing-delegation` | info | Fable 5.2, Mythos 5.2 | "always delegate", "use subagents for …": remove it if the model delegates more than the task needs |
| `hard-style-limit` | info | Fable 5.2, Mythos 5.2 | "one instruction per sentence", "sentences under 20 words": prose turns choppy |
| `routine-approval` | info | Fable 5.2, Mythos 5.2 | approval asked before routine steps (sending messages, reading, every tool call) |
| `ownership-framing` | info | Fable 5.2, Mythos 5.2 | "you are the owner and long-term maintainer of this codebase": more output and unrequested changes. Keep it only where you want them |
| `unscoped-absolute` | info | every model | a sentence that starts with "never" or "always" and gives no scope or reason |

The rules try hard not to flag what isn't an instruction:

- Text in code fences, inline code, and double quotes is what the instructions show, not
  what they say, so it isn't checked.
- A paragraph or list item is read as one piece, with its wrapped lines joined, so a
  reason on the next line counts.
- `unscoped-absolute` flags a sentence only when it starts with "never" or "always" (or
  "you must never …"), has no reason or scope in it ("because", "unless", "when",
  "before", "only", a colon, a parenthesis, …), and is the last sentence of its item. A
  sentence followed by more in the same item usually explains itself. It's checked only
  where no other rule matched.

The rules live in
[`lint_rules.toml`](https://github.com/unionai-oss/hlock/blob/main/crates/harness-lock-core/src/lint_rules.toml),
each with examples it must flag and counterexamples it mustn't, which the tests check.

## In CI

Warnings fail `hlock lint --ci`; info findings never fail it. `--format json` and
`--format sarif` (for GitHub code scanning, rule ids `hlock/lint/<rule>`) work as in
[`hlock audit`](audit.md#hlock-audit):

```yaml
      - run: uvx hlock lint --ci --model claude-fable-5-2 --file AGENTS.md
```

## From Python

`hlock.lint(...)` returns the report as JSON, and `hlock.lint_text(text, file, model)` lints
text you hold. Each finding carries the byte span of the match (`start`, `end`) and of the
whole lines it's on (`line_start`, `line_end`), so a tool can delete or rewrite exactly
those lines. See the [Python API](../reference/python-api.md#lint).
