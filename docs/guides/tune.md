# Tuning

`hlock resolve` *chooses* among the components you list. `hlock tune` goes further
and *produces* improvements: tuned prompts, tuned hyperparameters, and, at the deepest
levels, fine-tuned small models. The result goes into a separate `harness.tune.lock`, so
your base lock stays untouched until you adopt it.

```bash
hlock tune --depth hpo
```

```console
✓ wrote harness.tune.lock
╭─ ⚙ tune ────────────────────────────╮
│                                     │
│  depth    hpo                       │
│  backend  local                     │
│  data     user-traces               │
│  grid     12 configurations         │
│                                     │
│  Best configuration                 │
│  model                 qwen-7b      │
│  retries               3            │
│  temperature           0.7          │
│  tool timeout seconds  120          │
│                                     │
│  success rate  100%  →  100%   +0%  │
│                                     │
╰─────────────────────────────────────╯
next → hlock tune --apply
```

The baseline is the success rate recorded in your current `harness.lock`.

## Depth

`--depth` sets how far to go. Each level includes everything before it.

| Depth | Adds | Search grid |
|---|---|---|
| `probe` | Cheap policy probes, no retraining | models × retries `[0]` |
| `prompt` | System prompt and instructions search | models × retries `[0, 1]` |
| `hpo` | Hyperparameter search | models × retries `[0, 1, 3]` × temperature `[0.2, 0.7]` |
| `ft` | LoRA fine-tuning of a small model on your data | `hpo` grid, plus the newest model |
| `full` | Holdout re-validation and a tool-timeout sweep | `ft` grid × tool timeout `[30, 120]` s |
| `auto` *(default)* | Currently the same as `hpo` | |

`ft` and `full` need the `devbox` or `cluster` [backend](backends.md) for real
training, and a GPU for anything beyond a tiny model.

## Data

`--data` sets what tuning learns from:

| Value | Source |
|---|---|
| `user` *(default)* | Traces found in the current directory |
| `user:<paths>` | Comma-separated trace files or directories (CSV, JSONL, Parquet) |
| `synthetic[:n]` | `n` generated variants per task: failure seeding, tool-error recovery, prompt-injection attempts |
| `hub:<pack>` | A published trace pack *(roadmap)* |

Traces are normalized, redacted, split into train, validation, and test shards, and
content-hashed, so the tune lock records exactly what data produced it.

## Adopting the result

```bash
hlock tune --depth hpo --apply     # also write the result into harness.lock
hlock tune --depth hpo --strict    # fail unless it beats the base lock
```

`--strict` makes `tune` safe to run on a schedule: it exits non-zero, and writes no
tune lock, unless the tuned stack beats your current success rate.

## Other options

| Option | Description |
|---|---|
| `--harness NAME` | Restrict the search to one harness |
| `--max-trials N` | Cap the number of trials |
| `--out FILE` | Write somewhere other than `harness.tune.lock` |
| `--backend` | Override the manifest's backend |
| `--workdir DIR` | Where trial artifacts go (default `.harnesslock`) |

## Fine-tuning settings

LoRA fine-tuning (`--depth ft` and `full`) reads these environment variables:

| Variable | Default | Description |
|---|---|---|
| `HL_FT_BASE_MODEL` | `Qwen/Qwen2-0.5B` | Base model to fine-tune |
| `HL_FT_STEPS` | `3` | Training steps |
| `HL_GPU_COST_PER_H` | `2.0` | GPU price per hour in USD, used for cost evidence |
