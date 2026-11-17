# Upgrading components

New models come out all the time. `hlock upgrade` tells you which locked components
have newer versions available, re-measures the stack with them, and only changes your
lock when you ask. It always keeps the previous lock, so you can roll back.

`upgrade` works on a [resolved lock](resolve.md), because it re-measures against your
tasks. To update a [pinned lock](pin.md), change your setup (install a newer harness,
switch the model), then run `hlock pin` again.

## Plan

By default `upgrade` only plans:

```console
$ hlock upgrade --all
component  locked      available
planner    qwen-7b  →  qwen-32b
plan: 1 upgrade(s) available — rerun with --apply to perform.
next → hlock upgrade --all --apply
```

A locked [decision model](deciders.md) with a newer version is listed too, and `--apply`
re-resolves with it:

```console
$ hlock upgrade --all
component       locked                  available
planner         qwen-7b              →  qwen-32b
decider · stop  typesafe:jev-1.13.0  →  typesafe:jev-1.14.0
plan: 2 upgrade(s) available — rerun with --apply to perform.
next → hlock upgrade --all --apply
```

Upgrade one target instead of everything with `--to`:

```bash
hlock upgrade --to qwen-32b
```

## Apply

```console
$ hlock upgrade --all --apply
component  locked      available
planner    qwen-7b  →  qwen-32b
✓ upgraded  success rate 100% → 100%  (previous lock kept at harness.lock.prev)
  cost/task  $0.0001 → $0.0004 +300%   output tokens  50 → 50 +0%
```

Applying re-runs the eval pack on the upgraded stack and compares success rates. If
the success rate drops, the upgrade is rolled back automatically. Pass `--force` to
keep it anyway.

The line under it is what the upgrade costs: the mean cost and output tokens per task,
before and after, from the two locks' evidence. A side that wasn't measured shows `—`.
Here the upgrade held the success rate at four times the cost.

## Cost gate

A new model can hold the success rate and still cost much more: more tokens per task,
more tool calls, longer turns. Set a limit in `[gate]`, and an upgrade over it is rolled
back like one that lowers the success rate:

```toml title="harness.toml"
[gate]
max-cost-increase = 0.2     # at most 20% more cost per task
max-token-increase = 0.5    # at most 50% more output tokens per task (optional)
```

```console
$ hlock upgrade --all --apply
component  locked      available
planner    qwen-7b  →  qwen-32b
upgrade held success-rate (1.00 -> 1.00) but cost per task rose from $0.0001 to $0.0004 (+300%), more than the 20% max-cost-increase allows ([gate] in harness.toml), so it was rolled back; use --force to apply anyway.
  cost/task  $0.0001 → $0.0004 +300%   output tokens  50 → 50 +0%
```

`max-cost-increase` compares the mean cost per task. When a side's cost isn't known (the
endpoint reports no price), it compares output tokens per task instead. `max-token-increase`
always compares output tokens. A limit that can't be checked, because neither side's
evidence has the number, doesn't block the upgrade; `upgrade` says it wasn't checked.
Without `[gate]`, nothing is rejected for cost. `--force` skips both gates.

`[gate]` also holds [`hlock tune`](tune.md#cost-limits): a configuration or edit over it
isn't picked.

## Roll back

```console
$ hlock upgrade --rollback
✓ restored harness.lock from harness.lock.prev
```

## Reviewing an upgrade

Because the lock is a committed file, an upgrade is a reviewable diff:

```bash
hlock upgrade --all --apply
git diff harness.lock     # model, fingerprint, and evidence changes side by side
hlock diff                # the same, against harness.lock.prev, with the evidence change
```
