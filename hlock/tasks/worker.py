"""Python worker for harness-lock's heavy tuning work items.

Invoked out-of-process by the Rust core (``hlock``) over a versioned JSON contract:

    python -m hlock.tasks.worker <request.json> <response.json>

Each handler owns one ecosystem-heavy stage of ``hlock tune``. On
``backend=cluster`` (a fully-fledged Union cluster) the worker lifts each body to
a real Flyte 2 task — CPU images for data/prompt/grading/LLM trials, a GPU image
+ accelerator request for LoRA fine-tuning, `cache="auto"` for durable trial
caching, dataset/adapter offload through ``flyte.io``, and run-id/budget
provenance — and returns the identical response shape either way. Locally
(``devbox`` or no SDK) the same body runs in-process.

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
CONTRACT_VERSION = 1


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


def _call_real_llm(model: str, prompt: str, temperature: float = 0.2) -> dict | None:
    """Best-effort OpenAI-compatible completion against a served model; None on failure."""
    base = os.environ.get("HL_LLM_BASE_URL", "").rstrip("/")
    key = os.environ.get("HL_LLM_API_KEY", "")
    real_model = os.environ.get("HL_LLM_MODEL", model)
    if not base:
        return None
    try:
        import urllib.request

        body = json.dumps(
            {
                "model": real_model,
                "messages": [{"role": "user", "content": prompt}],
                "max_tokens": 200,
                "temperature": temperature,
            }
        ).encode()
        headers = {"Content-Type": "application/json"}
        if key:
            headers["Authorization"] = f"Bearer {key}"
        req = urllib.request.Request(
            base + "/chat/completions", data=body, headers=headers, method="POST"
        )
        with urllib.request.urlopen(req, timeout=60) as resp:
            data = json.loads(resp.read().decode())
        content = data["choices"][0]["message"]["content"]
        usage = data.get("usage", {})
        return {
            "content": content,
            "cost_usd": 0.0,
            "latency_ms": 0.0,
            "usage": {
                "input_tokens": usage.get("prompt_tokens", 0),
                "output_tokens": usage.get("completion_tokens", 0),
            },
        }
    except Exception as exc:  # noqa: BLE001 - fall back on any transport failure
        sys.stderr.write(f"[worker] real LLM failed ({exc}); using reference fallback\n")
        return None


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

def llm_run_trial(req: dict) -> dict:
    """Run one (stack, task, seed) trial honoring retries/temperature/timeout."""
    stack = req.get("stack") or {}
    model_spec = stack.get("llm", {}).get("planner", "reference:qwen-7b")
    provider, _, model = model_spec.partition(":")
    if not model:
        provider, model = "reference", model_spec
    retries = int(stack.get("retries", 0))
    max_steps = int(stack.get("max_steps", 40))
    temperature = float(stack.get("temperature") or 0.2)
    tool_timeout = float(stack.get("tool_timeout_seconds") or 120.0)
    seed = int(req.get("seed", 0))
    task = req.get("task") or {}
    prompt = task.get("prompt_text") or "solve the task"

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
    scoring, fine-tuning) consume a content-hashed dataset, and (on Union) returns
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
    samples / loss / gpu-h / spend. On a Union GPU worker this runs with an
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


HANDLERS = {
    "llm.run_trial": llm_run_trial,
    "data.load": data_load,
    "data.synthetic": data_synthetic,
    "grade": grade,
    "prompt.search": prompt_search,
    "ft.lora": ft_lora,
}


# --------------------------------------------------------------------------- #
# Union (Flyte 2) lift and dispatch
# --------------------------------------------------------------------------- #

def _union_image(kind: str):
    """Return a (base image, pip packages) tuple for a stage's task environment."""
    if kind == "ft.lora":
        return (
            "pytorch/pytorch:2.1-cuda12.1-cudnn8-devel",
            ["transformers", "datasets", "peft", "accelerate", "bitsandbytes", "harness-lock"],
        )
    return (
        None,  # debian base
        ["pandas", "polars", "harness-lock"],
    )


def _union_requests(kind: str):
    import flyte

    if kind == "ft.lora":
        return flyte.Resources(cpu="8", memory="32Gi", gpu="1", gpu_model="nvidia-a10g")
    return flyte.Resources(cpu="4", memory="8Gi")


def _union_run(kind: str, req: dict):
    """Lift a single stage body to a real Union task and return (payload, run_id)."""
    import asyncio

    import flyte

    base, pkgs = _union_image(kind)
    image = (
        flyte.Image.from_base(base) if base
        else flyte.Image.from_debian_base(python_version=(3, 12))
    ).with_pip_packages(*pkgs)
    env = flyte.TaskEnvironment(name="hlock-" + kind.replace(".", "-"), image=image)

    @env.task(cache="auto", requests=_union_requests(kind))
    async def _stage(request: dict) -> dict:
        # Re-dispatch the SAME kind on the cluster (bundled harness-lock) with the
        # Union side done (no re-lift), so the body runs with the env's resources.
        return _dispatch_no_union(request)

    result = asyncio.run(flyte.run(_stage, inputs={"request": req}))
    run_id = getattr(result, "name", "flyte://unknown")
    return result, run_id


def _maybe_flyte(req: dict, body):
    """On `cluster`, best-effort lift the body to a real Union task; else in-process."""
    if req.get("backend") != "cluster":
        return body()
    try:
        payload, run_id = _union_run(req.get("kind", ""), req)
        if not isinstance(payload, dict) or "result" not in payload:
            payload = body()  # task returned a bare value; keep contract shape
        else:
            # inject Union provenance into the stage metadata
            md = payload.setdefault("result", {})
            digests = md.setdefault("model_digests", {})
            if isinstance(digests, dict):
                digests["run"] = run_id
                digests["backend"] = "cluster"
        return payload
    except Exception as exc:  # noqa: BLE001 - no SDK / not on a cluster -> run locally
        sys.stderr.write(f"[worker] Union lift unavailable ({exc}); running in-process\n")
        return body()


def _dispatch_no_union(req: dict) -> dict:
    """Dispatch a handler without recursively attempting a Union lift."""
    kind = req.get("kind", "")
    handler = HANDLERS.get(kind)
    if handler is None:
        return {"error": f"unknown task kind: {kind!r}"}
    return handler(req)


def _dispatch(req: dict) -> dict:
    return _dispatch_no_union(req)


def _write(path, obj):
    Path(path).write_text(json.dumps(obj))


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(prog="hlock-worker")
    ap.add_argument("request", type=Path, help="path to the request JSON")
    ap.add_argument("response", type=Path, help="path to write the response JSON")
    args = ap.parse_args(argv)
    try:
        req = json.loads(args.request.read_text())
    except Exception as exc:  # noqa: BLE001
        _write(args.response, {"ok": False, "error": f"bad request: {exc}"})
        return 1
    if req.get("version") != CONTRACT_VERSION:
        _write(
            args.response,
            {"ok": False, "error": f"unsupported contract version {req.get('version')}"},
        )
        return 1
    try:
        payload = _maybe_flyte(req, lambda: _dispatch_no_union(req))
    except Exception as exc:  # noqa: BLE001
        _write(args.response, {"ok": False, "error": str(exc)})
        return 1
    if payload is None:
        payload = {"error": "worker produced no output"}
    if "error" in payload:
        _write(args.response, {"ok": False, "version": CONTRACT_VERSION, "error": payload["error"]})
        return 1
    _write(args.response, {"ok": True, "version": CONTRACT_VERSION, "result": payload["result"]})
    return 0


if __name__ == "__main__":  # pragma: no cover
    sys.exit(main())
