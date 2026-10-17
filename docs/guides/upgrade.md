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
```

Applying re-runs the eval pack on the upgraded stack and compares success rates. If
the success rate drops, the upgrade is rolled back automatically. Pass `--force` to
keep it anyway.

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
```
