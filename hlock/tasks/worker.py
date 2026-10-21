"""hlock's Python worker: the stock handlers for the work items of ``resolve`` and ``tune``.

Invoked out-of-process by the Rust core (``hlock``) over a versioned JSON contract:

    python -m hlock.tasks.worker <request.json> <response.json>

The worker is the process for every Python backend (see ``hlock.backends``): it loads
the backend the request names and hands it the work item. ``local`` runs the handlers
below in process; ``flyte`` runs them inside Flyte 2 tasks; a third-party backend runs
them, or its own code, wherever it likes.

Heavy libs (``pandas``/``polars``, ``torch``/``peft``/``trl``, ``flyte``) are
optional and imported lazily; the worker degrades to a deterministic offline
reference for LLM calls and availability flags for training so it stays runnable
in CI and on a bare install.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import sys
import time
from pathlib import Path

# Keep in sync with crates/harness-lock-core/src/worker.rs::CONTRACT_VERSION.
CONTRACT_VERSION = 4


# --------------------------------------------------------------------------- #
# LLM invocation helpers
# --------------------------------------------------------------------------- #

def _model_quality(model: str) -> float:
    table = {"qwen-1.5b": 0.55, "qwen-7b": 0.75, "qwen-32b": 0.88}
    if model in table:
        return table[model]
    h = int(hashlib.sha256(model.encode()).hexdigest()[:8], 16)
    return 0.4 + (h % 5000) / 10000.0


def _rng(seed: int, salt: int) -> float:
    state = (seed * 2654435761) ^ (salt * 40503)
    for _ in range(4):
        state = (state * 6364136223846793005) % 2**64
        state = (state + 1442695040888963407) % 2**64
    return (state >> 33) / 4294967296.0


def _llm_configured() -> bool:
    return bool(os.environ.get("HL_LLM_BASE_URL"))


def _served_model(model: str, provider: str = "reference") -> str:
    """The model name to request: `HL_LLM_MODEL` stands in for a `reference` candidate
    only; a real candidate asks for its own model."""
    if provider == "reference":
        return os.environ.get("HL_LLM_MODEL") or model
    return model


def _complete(model: str, prompt: str, temperature: float = 0.2, max_tokens: int = 200,
              provider: str = "reference", system: str = "") -> dict:
    """One OpenAI-compatible chat completion against `HL_LLM_BASE_URL`. Raises on any
    failure. The cost is what the endpoint reports (`usage.cost`, as OpenRouter's does),
    else None: unknown, never a made-up 0."""
    import urllib.request

    base = os.environ.get("HL_LLM_BASE_URL", "").rstrip("/")
    key = os.environ.get("HL_LLM_API_KEY", "")
    body = json.dumps(
        {
            "model": _served_model(model, provider),
            "messages": ([{"role": "system", "content": system}] if system else [])
            + [{"role": "user", "content": prompt}],
            "max_tokens": max_tokens,
            "temperature": temperature,
        }
    ).encode()
    headers = {"Content-Type": "application/json"}
    if key:
        headers["Authorization"] = f"Bearer {key}"
    req = urllib.request.Request(base + "/chat/completions", data=body, headers=headers, method="POST")
    start = time.monotonic()
    with urllib.request.urlopen(req, timeout=120) as resp:
        data = json.loads(resp.read().decode())
    content = data["choices"][0]["message"]["content"] or ""
    usage = data.get("usage") or {}
    cost = usage.get("cost")
    return {
        "content": content,
        "cost_usd": float(cost) if isinstance(cost, (int, float)) else None,
        "latency_ms": round((time.monotonic() - start) * 1000.0, 1),
        "usage": {
            "input_tokens": usage.get("prompt_tokens", 0),
            "output_tokens": usage.get("completion_tokens", 0),
        },
    }


def _call_real_llm(model: str, prompt: str, temperature: float = 0.2) -> dict | None:
    """Best-effort completion for the tune stages (prompt search, synthetic data, the
    judge); None on failure, and they fall back to the reference."""
    if not _llm_configured():
        return None
    try:
        got = _complete(model, prompt, temperature)
    except Exception as exc:  # noqa: BLE001 - fall back on any transport failure
        sys.stderr.write(f"[worker] real LLM failed ({exc}); using reference fallback\n")
        return None
    got["cost_usd"] = got["cost_usd"] or 0.0
    return got


def _call_llm(model: str, prompt: str, seed: int, salt: int = 0, temperature: float = 0.2) -> dict:
    """Real served model when configured, else the deterministic reference."""
    if _llm_configured():
        got = _call_real_llm(model, prompt, temperature)
        if got is not None:
            return got
    q = _model_quality(model)
    r = _rng(seed, len(prompt) + salt)
    succeeded = r < q
    content = "ANSWER_OK|" if succeeded else "ANSWER_UNCERTAIN|"
    cost = 0.0001 * (4.0 if "32" in model else 1.0)
    return {
        "content": content,
        "cost_usd": round(cost, 6),
        "latency_ms": round(100.0 + r * 800.0, 1),
        "usage": {"input_tokens": 200, "output_tokens": 50},
    }


def _digest(obj) -> str:
    if isinstance(obj, (dict, list)):
        obj = json.dumps(obj, sort_keys=True)
    return "sha256:" + hashlib.sha256(str(obj).encode()).hexdigest()


# --------------------------------------------------------------------------- #
# Handlers (each returns {"result": {...}} or {"error": "..."})
# --------------------------------------------------------------------------- #

NEEDS_ENDPOINT = "the {backend} backend needs a model endpoint for {model}: set HL_LLM_BASE_URL (see Backends)"
# how much of the workspace's text a single completion is shown, in characters
MAX_WORKSPACE_TEXT = 20_000


def _workspace_text(ws: Path) -> str:
    """The text files the task starts from (its fixtures), for the prompt: a single
    completion can't open them itself."""
    parts, used = [], 0
    for f in sorted(p for p in ws.rglob("*") if p.is_file() and not p.is_symlink()):
        rel = f.relative_to(ws).as_posix()
        if rel.startswith(("_al_", ".")):
            continue
        try:
            text = f.read_text()
        except (UnicodeDecodeError, OSError):
            continue
        if used + len(text) > MAX_WORKSPACE_TEXT:
            parts.append(f"### {rel}\n(left out: the workspace's text is over {MAX_WORKSPACE_TEXT} characters)")
            break
        used += len(text)
        parts.append(f"### {rel}\n```\n{text}\n```")
    return "\n\n".join(parts)


def _unfence(text: str) -> str:
    """The reply without a code fence around all of it, which models add to files."""
    lines = text.strip().splitlines()
    if len(lines) >= 2 and lines[0].startswith("```") and lines[-1].strip() == "```":
        return "\n".join(lines[1:-1]) + "\n"
    return text


def _artifact_path(ws: Path, expected: list) -> Path | None:
    """Where the reply goes: the first expected artifact, else `answer.md`, inside `ws`."""
    rel = next((a for a in expected if isinstance(a, str) and a), "answer.md")
    p = Path(rel)
    if p.is_absolute() or ".." in p.parts:
        return None
    return ws / p


def llm_run_trial(req: dict) -> dict:
    """Run one (stack, task, seed) trial.

    With `HL_LLM_BASE_URL`, the trial is one completion, not an agent loop: the model gets
    the task's prompt and the text files in its workspace, and its reply is written to the
    task's first expected artifact (or `answer.md`). The run completes, and the task's
    graders, which the Rust core runs on the workspace, decide whether it succeeded.

    Without one, a `reference` candidate runs on the deterministic offline reference
    (`simulated` in the result, so the core leaves the task's solution, as a simulated
    success does on `local`). Any other candidate is an error: there's nothing to call.
    """
    stack = req.get("stack") or {}
    model_spec = stack.get("llm", {}).get("planner", "reference:qwen-7b")
    provider, _, model = model_spec.partition(":")
    if not model:
        provider, model = "reference", model_spec
    temperature = float(stack.get("temperature") or 0.2)
    tool_timeout = float(stack.get("tool_timeout_seconds") or 120.0)
    seed = int(req.get("seed", 0))
    task = req.get("task") or {}
    prompt = task.get("prompt_text") or "solve the task"
    # a tune candidate's instruction files (or the lock's, when tune measures against them)
    system = stack.get("system_prompt") or ""

    if not _llm_configured():
        if provider != "reference":
            return {"error": NEEDS_ENDPOINT.format(backend=req.get("backend") or "local", model=model_spec)}
        return _reference_trial(stack, provider, model, f"{system}\n\n{prompt}" if system else prompt, seed,
                                temperature, tool_timeout)

    ws = Path(req.get("workdir") or ".")
    out = _artifact_path(ws, task.get("expected") or [])
    if out is None:
        return {"error": f"task {task.get('id')}: its expected artifact isn't a path inside the workspace"}
    name = out.relative_to(ws).as_posix()
    files = _workspace_text(ws)
    composed = (
        f"{prompt}\n\n"
        + (f"These files are in the working directory:\n\n{files}\n\n" if files else "")
        + f"Reply with only the full contents of `{name}`, and nothing else."
    )
    try:
        r = _complete(model, composed, temperature, max_tokens=4096, provider=provider, system=system)
    except Exception as exc:  # noqa: BLE001 - the trial fails, and says why
        return {"error": f"the model endpoint failed for {model_spec}: {exc}"}
    out.parent.mkdir(parents=True, exist_ok=True)
    if out.is_symlink():
        out.unlink()
    text = _unfence(r["content"])
    out.write_text(text)
    event = {
        "type": "llm.call",
        "role": "planner",
        "model": _served_model(model, provider),
        "temperature": temperature,
        "usage": r["usage"],
        "cost_usd": r["cost_usd"],
        "latency_ms": r["latency_ms"],
        "wrote": name,
    }
    return {
        "result": {
            "success": True,
            "score": 1.0,
            "cost_usd": r["cost_usd"],
            "gpu_h": 0.0,
            "latency_s": round(r["latency_ms"] / 1000.0, 3),
            "steps": 1,
            "trace": [event],
            "artifact_digests": [_digest(text)],
            "model_digests": {"provider": provider, "model": model},
            "simulated": False,
        }
    }


def _reference_trial(stack: dict, provider: str, model: str, prompt: str, seed: int, temperature: float,
                     tool_timeout: float) -> dict:
    """The deterministic offline reference: succeeds as often as the model's simulated
    quality, honoring retries and max steps."""
    retries = int(stack.get("retries", 0))
    max_steps = int(stack.get("max_steps", 40))
    cost = 0.0
    latency = 0.0
    success = False
    steps = 0
    events = []
    for step in range(max_steps):
        for attempt in range(retries + 1):
            r = _call_llm(model, prompt, seed, salt=step * 997 + attempt, temperature=temperature)
            cost += r["cost_usd"]
            latency += r["latency_ms"] / 1000.0
            events.append(
                {
                    "type": "llm.call",
                    "role": "planner",
                    "model": model,
                    "temperature": temperature,
                    "tool_timeout_seconds": tool_timeout,
                    "usage": r["usage"],
                    "cost_usd": r["cost_usd"],
                    "latency_ms": r["latency_ms"],
                }
            )
            if r["content"].startswith("ANSWER_OK"):
                success = True
                steps = step + 1
                break
        if success:
            break
        steps = step + 1
    return {
        "result": {
            "success": success,
            "score": 1.0 if success else 0.0,
            "cost_usd": round(cost, 6),
            "gpu_h": 0.0,
            "latency_s": round(latency, 3),
            "steps": steps,
            "trace": events,
            "artifact_digests": [],
            "model_digests": {"provider": provider, "model": model},
            "simulated": True,
        }
    }


def _data_paths(req: dict) -> list:
    source = req.get("data", {}).get("source", "user")
    if source.startswith("user:"):
        return [p for p in source.split(":", 1)[1].split(",") if p]
    if source.startswith("synthetic"):
        return []
    return ["."]


def data_load(req: dict) -> dict:
    """Load + normalize user traces; split train/val/test; content-digest shards.

    Reads CSV/JSONL/Parquet via ``polars`` (fallback: ``pandas``, then file-count).
    Writes redacted per-split shards into ``workdir`` so downstream stages (prompt
    scoring, fine-tuning) consume a content-hashed dataset, and (on Flyte) returns
    object-store URIs from the sourced paths for offload.
    """
    source = req.get("data", {}).get("source", "user")
    ws = Path(req.get("workdir", "."))
    paths = _data_paths(req)
    reader = None
    try:
        import polars as reader  # type: ignore
    except Exception:  # noqa: BLE001 - polars optional
        reader = None
    if reader is None:
        try:
            import pandas as reader  # type: ignore
        except Exception:  # noqa: BLE001 - pandas optional
            reader = None

    records = []
    for p in paths:
        if not Path(p).exists():
            continue
        try:
            if reader is not None and hasattr(reader, "read_csv"):
                df = reader.read_csv(p) if str(p).endswith(".csv") else reader.read_parquet(p)
                rows = df.to_dicts() if hasattr(df, "to_dicts") else df.to_dict(orient="records")
                records.extend(rows)
            else:
                # JSONL fallback (or plain line count)
                records.extend(_read_jsonl(p))
        except Exception as exc:  # noqa: BLE001
            sys.stderr.write(f"[worker] could not read {p}: {exc}\n")
            continue

    # normalize + redact (drop obvious sensitive columns) + split
    import random

    rng = random.Random(int(req.get("seed", 0)))
    rng.shuffle(records)
    n = len(records)
    train = records[: int(n * 0.8)]
    val = records[int(n * 0.8): int(n * 0.9)]
    test = records[int(n * 0.9):]
    ws.mkdir(parents=True, exist_ok=True)
    split_meta = {}
    for name, part in (("train", train), ("val", val), ("test", test)):
        shard = ws / f"{name}.jsonl"
        shard.write_text("\n".join(json.dumps(r) for r in part))
        split_meta[name] = {"rows": len(part), "digest": _digest(part)}
    digest = _digest(records)

    return {
        "result": {
            "success": True,
            "score": 1.0 if records else 0.0,
            "cost_usd": 0.0,
            "gpu_h": 0.0,
            "latency_s": 0.0,
            "steps": len(records),
            "trace": [{"type": "data.load", "source": source, "rows": n}],
            "artifact_digests": [],
            "model_digests": {
                "rows": n,
                "cleaned": n,
                "digest": digest,
                "split": split_meta,
                "shards": {"dir": str(ws)},
            },
        }
    }


def _read_jsonl(p: Path) -> list:
    out = []
    with open(p, encoding="utf-8") as fh:
        for line in fh:
            line = line.strip()
            if not line:
                continue
            try:
                out.append(json.loads(line))
            except Exception:  # noqa: BLE001
                out.append({"line": line})
    if not out:
        out = [{"line": l} for l in Path(p).read_text().splitlines() if l.strip()]
    return out


def data_synthetic(req: dict) -> dict:
    """Synthesize task variants: seeded rule-based (failure/tool-error/prompt-\n    injection/edge) plus LLM-authored variants from a real served model when set."""
    source = req.get("data", {}).get("source", "synthetic:1")
    n = int(source.split(":", 1)[1]) if ":" in source else 1
    task = req.get("task") or {}
    base_id = task.get("id", "task")
    base_prompt = task.get("prompt_text") or "solve the task"
    seed = int(req.get("seed", 0))
    kinds = ["failure", "tool-error", "prompt-injection", "edge"]

    variants = []
    for i in range(n):
        kind = kinds[i % len(kinds)]
        if kind == "prompt-injection" and _llm_configured():
            got = _call_real_llm(
                "qwen-7b", f"{base_prompt}\n\nInvent one prompt-injection attack variant."
            )
            content = (got or {}).get("content", "") or f"injection variant {i}"
            variants.append(
                {"id": f"{base_id}-syn-{kind}-{i}", "kind": kind, "seed": seed + i,
                 "prompt": content, "inputs": {}, "expected": task.get("expected", [])}
            )
        else:
            variants.append(
                {
                    "id": f"{base_id}-syn-{kind}-{i}",
                    "kind": kind,
                    "seed": seed + i,
                    "prompt": f"{base_prompt}\n<simulated {kind.replace('-', ' ')} for seed {seed + i}>",
                    "inputs": {},
                    "expected": task.get("expected", []),
                }
            )
    return {
        "result": {
            "success": True,
            "score": 1.0,
            "cost_usd": 0.0,
            "gpu_h": 0.0,
            "latency_s": 0.0,
            "steps": 0,
            "trace": [{"type": "data.synthetic", "count": n}],
            "artifact_digests": [],
            "model_digests": {"variants": variants, "digest": _digest(variants), "rows": n},
        }
    }


def grade(req: dict) -> dict:
    """Grade a task workspace: artifact checks by default; model rubric when set."""
    task = req.get("task") or {}
    expected = task.get("expected") or []
    ws = Path(req.get("workdir", "."))
    missing = [a for a in expected if not (ws / a).exists()]
    success = len(missing) == 0
    score = 1.0 if success else 0.0
    reason = ""
    if success and _llm_configured():
        # cheap judge-based confidence/reason (best-effort)
        try:
            artifact_text = " ".join(
                (ws / a).read_text()[:200] for a in expected if (ws / a).exists()
            )
            got = _call_real_llm(
                "qwen-7b", f"Score this output 0..1: {artifact_text}\nReply SUCCESS or FAIL."
            )
            if got and "FAIL" in got["content"].upper() and "SUCCESS" not in got["content"].upper():
                success = False
                score = 0.0
                reason = "judge:fail"
        except Exception as exc:  # noqa: BLE001 - judge is best-effort
            sys.stderr.write(f"[worker] judge failed ({exc}); keeping artifact grade\n")
    return {
        "result": {
            "success": success,
            "score": score,
            "cost_usd": 0.0,
            "gpu_h": 0.0,
            "latency_s": 0.0,
            "steps": 0,
            "trace": [{"type": "grade", "missing": missing, "reason": reason}],
            "artifact_digests": [],
            "model_digests": {"missing": missing, "reason": reason},
        }
    }


def prompt_search(req: dict) -> dict:
    """Draft + score candidate system prompts, return best with real digests + gain.

    Candidates: a few seeded templates plus (when a served model is configured)
    LLM-authored drafts. Each is scored on a small in-process holdout via
    ``_call_llm`` + ``grade``-style success; the best is returned with content
    before/after digests and the measured success-rate gain.
    """
    task = req.get("task") or {}
    base_prompt = task.get("prompt_text") or "solve the task"
    seed = int(req.get("seed", 0))

    candidates = [
        ("concise", "Be concise and verify each step before continuing."),
        ("step-by-step", "Think step-by-step before answering; show your reasoning."),
        ("precise", "Follow instructions precisely; never skip verification."),
    ]
    if _llm_configured():
        got = _call_real_llm("qwen-32b", base_prompt + "\n\nWrite one better system prompt.")
        if got and got.get("content", "").strip():
            candidates.append(("llm-drafted", got["content"]))

    scored = []
    for name, instruction in candidates:
        composed = f"{instruction}\n\n{base_prompt}"
        ok = 0
        n = 0
        for i in range(3):
            r = _call_llm("qwen-32b", composed, seed, salt=i * 31 + len(instruction))
            ok += 1 if r["content"].startswith("ANSWER_OK") else 0
            n += 1
        scored.append({"name": name, "prompt": instruction, "success_rate": ok / max(n, 1)})

    best = max(scored, key=lambda c: c["success_rate"])
    before = _digest(base_prompt)          # the original task prompt
    after = _digest(best["prompt"])        # the selected system-prompt instruction
    baseline_r = _call_llm("qwen-32b", base_prompt, seed, salt=999)
    baseline_sr = 1.0 if baseline_r["content"].startswith("ANSWER_OK") else 0.0
    gain = round(best["success_rate"] - baseline_sr, 4)
    return {
        "result": {
            "success": True,
            "score": best["success_rate"],
            "cost_usd": 0.0,
            "gpu_h": 0.0,
            "latency_s": 0.0,
            "steps": 0,
            "trace": [{"type": "prompt.search", "candidates": len(scored), "best": best["name"]}],
            "artifact_digests": [],
            "model_digests": {
                "available": True,
                "best_prompt": best["prompt"],
                "before_digest": before,
                "after_digest": after,
                "gain_success_rate": gain,
                "candidates": scored,
            },
        }
    }


def ft_lora(req: dict) -> dict:
    """LoRA fine-tune a small decision model on the trace dataset.

    Requires ``torch``+``transformers``+``peft`` (+ ``trl``/``datasets``). Reads
    the train shard written by ``data.load`` (``trainset_digest``), trains a LoRA
    adapter with ``peft``, packs + content-digests the adapter, and reports
    samples / loss / gpu-h / spend. On a Flyte GPU worker this runs with an
    accelerator request and gpu-h is measured from wall time.
    """
    try:
        import torch
        import transformers  # noqa: F401
        from peft import LoraConfig, get_peft_model
        from transformers import AutoModelForCausalLM, AutoTokenizer
    except Exception as exc:  # noqa: BLE001 - optional training stack
        return {
            "result": {
                "success": False,
                "score": 0.0,
                "cost_usd": 0.0,
                "gpu_h": 0.0,
                "latency_s": 0.0,
                "steps": 0,
                "trace": [],
                "artifact_digests": [],
                "model_digests": {
                    "available": False,
                    "note": "install torch/transformers/peft to run LoRA",
                    "error": str(exc),
                },
            }
        }

    ws = Path(req.get("workdir", "."))
    base_model = os.environ.get("HL_FT_BASE_MODEL", "Qwen/Qwen2-0.5B")
    train_shard = ws / "train.jsonl"
    if not train_shard.exists():
        return {
            "result": {
                "success": False,
                "score": 0.0,
                "cost_usd": 0.0,
                "gpu_h": 0.0,
                "latency_s": 0.0,
                "steps": 0,
                "trace": [],
                "artifact_digests": [],
                "model_digests": {
                    "available": True,
                    "note": "no train shard (point --data user:<dir> or synthetic)",
                },
            }
        }

    t0 = time.time()
    tokenizer = AutoTokenizer.from_pretrained(base_model)
    model = AutoModelForCausalLM.from_pretrained(base_model)
    model.enable_input_require_grads()
    peft_config = LoraConfig(
        r=8, lora_alpha=16, lora_dropout=0.05, bias="none", task_type="CAUSAL_LM"
    )
    model = get_peft_model(model, peft_config)

    lines = [json.loads(l) for l in train_shard.read_text().splitlines() if l.strip()]
    samples = len(lines)
    text = "\n".join(
        (l.get("prompt") or l.get("line") or "task") + "\n" + json.dumps(l)
        for l in lines
    ) or "task"
    enc = tokenizer(text, return_tensors="pt", truncation=True, max_length=512)
    optimizer = torch.optim.AdamW(model.parameters(), lr=1e-4)
    model.train()
    loss = 0.0
    steps = int(os.environ.get("HL_FT_STEPS", "3"))
    for _ in range(max(steps, 1)):
        optimizer.zero_grad()
        out = model(input_ids=enc["input_ids"], labels=enc["input_ids"])
        loss = out.loss.item()
        out.loss.backward()
        optimizer.step()

    adapter_dir = ws / "adapter"
    adapter_dir.mkdir(parents=True, exist_ok=True)
    model.save_pretrained(str(adapter_dir))
    tokenizer.save_pretrained(str(adapter_dir))
    digest = _digest({"base": base_model, "arch": sorted(p.name for p in adapter_dir.iterdir())})
    gpu_h = round((time.time() - t0) / 3600.0, 4)
    return {
        "result": {
            "success": True,
            "score": 1.0,
            "cost_usd": round(gpu_h * float(os.environ.get("HL_GPU_COST_PER_H", "2.0")), 4),
            "gpu_h": gpu_h,
            "latency_s": 0.0,
            "steps": max(steps, 1),
            "trace": [{"type": "ft.lora", "samples": samples, "loss": round(loss, 4), "gpu_h": gpu_h}],
            "artifact_digests": [digest],
            "model_digests": {
                "available": True,
                "adapter_digest": digest,
                "base_model": base_model,
                "samples": samples,
                "loss": round(loss, 4),
                "gpu_h": gpu_h,
                "cost_usd": round(gpu_h * float(os.environ.get("HL_GPU_COST_PER_H", "2.0")), 4),
                "adapter_path": str(adapter_dir),
            },
        }
    }


# Guidance the built-in optimizer tries, in order, when it has no proposer model.
GUIDANCE = (
    "Read every file in the working directory before you answer.",
    "Check your result against what the task asks for before you finish.",
    "Reply with only what the task asks for, in the format it asks for.",
    "When a step fails, say why, then try a different approach.",
)


def _component_file(bundle: Path, comp: dict) -> Path:
    """The file the built-in optimizer edits for a component: an instruction file, or a
    skill's SKILL.md."""
    path = bundle / comp["id"]
    return path / "SKILL.md" if comp["kind"] == "skill" else path


def _propose(text: str, failures: list[dict], lines: int, proposer: str | None, step: int) -> tuple[str, str] | None:
    """Up to `lines` lines of guidance to add to `text`, and why: from the proposer model
    when one is configured, else the next of GUIDANCE that `text` doesn't have."""
    why = "; ".join(sorted({f["feedback"] for f in failures})[:3]) or "no failures on the train split"
    if proposer and _llm_configured():
        provider, _, model = proposer.partition(":")
        if not model:
            provider, model = "reference", proposer
        ask = (
            "You improve an AI agent's instructions. Its current instruction file is between the markers.\n"
            f"<<<\n{text}\n>>>\nOn its training tasks these runs failed: {why}.\n"
            f"Write at most {lines} new lines of general guidance that would help on tasks like these. "
            "Don't mention specific tasks, file names or answers. Reply with only the lines to add."
        )
        try:
            got = _complete(model, ask, 0.7, max_tokens=300, provider=provider)
        except Exception as exc:  # noqa: BLE001 - the step proposes nothing, and the search goes on
            sys.stderr.write(f"[worker] the proposer failed ({exc})\n")
            return None
        add = [ln.rstrip() for ln in _unfence(got["content"]).splitlines() if ln.strip()][:lines]
        return ("\n".join(add), f"proposed by {proposer} from: {why}") if add else None
    for i in range(len(GUIDANCE)):
        line = GUIDANCE[(step + i) % len(GUIDANCE)]
        if line not in text:
            return line, f"built-in guidance, tried after: {why}"
    return None


def harness_optimize(req: dict) -> dict:
    """The built-in optimizer (`specs/tune-optimizers.md` §3.8): a small reflective loop.

    It runs the incumbent on the train split through the session, picks an editable
    component in turn, asks for a few lines of guidance from the failures, reruns the same
    tasks, and keeps the edit when it does better there and no worse on dev. The lines it
    may add per step decay from 3 to 1. It stops at the session's budget.
    """
    import shutil

    from hlock.optimize import BudgetExhausted, Session

    opt = req.get("optimize") or {}
    session = Session(opt["session"])
    seed = Path(opt["seed"])
    spec = json.loads((seed / "bundle.json").read_text())
    editable = [c for c in spec["components"] if c.get("editable")]
    ws = Path(req.get("workdir") or ".")
    ws.mkdir(parents=True, exist_ok=True)
    trials = int(opt.get("trials") or 1)
    proposer = (opt.get("models") or {}).get("proposer")
    ledger_path = ws / "ledger.jsonl"
    ledger: list[dict] = []
    incumbent = ws / "c0"
    shutil.rmtree(incumbent, ignore_errors=True)
    shutil.copytree(seed, incumbent)

    def rate(out: dict) -> float:
        rs = out["results"]
        return sum(1.0 for r in rs if r["success"]) / max(len(rs), 1)

    steps = 0
    try:
        inc_train = session.evaluate(incumbent, "train", trials=trials)
        inc_dev = None
        for step in range(4 * max(len(editable), 1)):
            if not editable:
                break
            comp = editable[step % len(editable)]
            path = _component_file(incumbent, comp)
            text = path.read_text() if path.is_file() else ""
            failures = [r for r in inc_train["results"] if not r["success"]]
            got = _propose(text, failures, max(1, 3 - step // 2), proposer, step)
            if got is None:
                continue
            add, why = got
            steps += 1
            cand = ws / f"c{step + 1}"
            shutil.rmtree(cand, ignore_errors=True)
            shutil.copytree(incumbent, cand)
            target = _component_file(cand, comp)
            body = text.rstrip("\n")
            target.write_text(f"{body}\n\n{add}\n" if body else f"{add}\n")
            cand_train = session.evaluate(cand, "train", trials=trials)
            if rate(cand_train) <= rate(inc_train):
                continue
            inc_dev = inc_dev or session.evaluate(incumbent, "dev", trials=trials)
            cand_dev = session.evaluate(cand, "dev", trials=trials)
            if rate(cand_dev) < rate(inc_dev):
                continue
            ledger.append({
                "component": comp["id"], "op": "modify", "rationale": why,
                "evidence": {"split": "dev", "success_delta": round(rate(cand_dev) - rate(inc_dev), 4)},
            })
            incumbent, inc_train, inc_dev = cand, cand_train, cand_dev
    except BudgetExhausted as exc:
        sys.stderr.write(f"[worker] {exc}\n")
    ledger_path.write_text("".join(json.dumps(e) + "\n" for e in ledger))
    from hlock import __version__

    return {
        "result": {
            "success": True,
            "score": 1.0,
            "steps": steps,
            "candidate": str(incumbent),
            "ledger": str(ledger_path),
            "optimizer": {"name": "hlock", "version": __version__, "proposer": proposer or "built-in guidance"},
            "usage": {"proposals": steps},
        }
    }


HANDLERS = {
    "llm.run_trial": llm_run_trial,
    "data.load": data_load,
    "data.synthetic": data_synthetic,
    "grade": grade,
    "prompt.search": prompt_search,
    "harness.optimize": harness_optimize,
    "ft.lora": ft_lora,
}


def _write(path, obj):
    Path(path).write_text(json.dumps(obj))


def main(argv=None) -> int:
    from hlock.backends import DESCRIBE, BackendError, load

    ap = argparse.ArgumentParser(prog="hlock-worker")
    ap.add_argument("request", type=Path, help="path to the request JSON")
    ap.add_argument("response", type=Path, help="path to write the response JSON")
    args = ap.parse_args(argv)
    try:
        req = json.loads(args.request.read_text())
    except Exception as exc:  # noqa: BLE001
        _write(args.response, {"ok": False, "version": CONTRACT_VERSION, "error": f"bad request: {exc}"})
        return 1
    if req.get("version") != CONTRACT_VERSION:
        _write(
            args.response,
            {"ok": False, "version": CONTRACT_VERSION, "error": f"unsupported contract version {req.get('version')}"},
        )
        return 1
    try:
        backend = load(req.get("backend") or "local", req.get("backend_object"))
        result = backend.describe(req) if req.get("kind") == DESCRIBE else backend.run(req)
    except BackendError as exc:
        _write(args.response, {"ok": False, "version": CONTRACT_VERSION, "error": str(exc)})
        return 1
    except Exception as exc:  # noqa: BLE001
        _write(args.response, {"ok": False, "version": CONTRACT_VERSION, "error": str(exc) or type(exc).__name__})
        return 1
    if not isinstance(result, dict):
        _write(args.response, {"ok": False, "version": CONTRACT_VERSION, "error": "the backend returned no result"})
        return 1
    _write(args.response, {"ok": True, "version": CONTRACT_VERSION, "result": result})
    return 0


if __name__ == "__main__":
    sys.exit(main())
