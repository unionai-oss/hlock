"""Tests for the Rust <-> Python worker bridge (hlock.tasks.worker).

These prove the seam ``hlock tune --backend devbox|cluster`` uses: the Rust core
shells a trial out to the Python worker over a versioned JSON contract and
consumes the response. They run entirely offline — the worker's LLM call falls
back to the deterministic reference RNG when no ``HL_LLM_BASE_URL`` is set, and
the Flyte lift degrades to in-process when the ``flyte`` SDK is absent.
"""

from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path

import pytest

from hlock.tasks.worker import CONTRACT_VERSION

REPO = Path(__file__).resolve().parents[1]


def run_worker(req: dict) -> dict:
    """Invoke the worker exactly as the Rust core does."""
    import tempfile

    with tempfile.TemporaryDirectory() as td:
        td = Path(td)
        rq_path = td / "_al_request.json"
        rs_path = td / "_al_response.json"
        rq_path.write_text(json.dumps(req))
        proc = subprocess.run(
            [sys.executable, "-m", "hlock.tasks.worker", str(rq_path), str(rs_path)],
            capture_output=True,
            text=True,
            check=False,
        )
        if rs_path.exists():
            return json.loads(rs_path.read_text())
        assert proc.returncode == 0, proc.stderr
        raise AssertionError("worker wrote no response file")


def test_worker_contract_llm_trial():
    resp = run_worker(
        {
            "version": CONTRACT_VERSION,
            "kind": "llm.run_trial",
            "backend": "local",
            "stack": {"llm": {"planner": "reference:qwen-7b"}, "retries": 0, "max_steps": 4},
            "task": {"id": "t1", "prompt_text": "Do the thing"},
            "seed": 0,
            "workdir": ".",
            "plan": {},
        }
    )
    assert resp["ok"] is True, resp
    r = resp["result"]
    assert "success" in r and "score" in r
    assert isinstance(r["success"], bool)
    assert isinstance(r["cost_usd"], float)
    assert isinstance(r["steps"], int)
    assert r["trace"]  # at least one llm.call event


def test_worker_contract_unknown_kind():
    resp = run_worker({"version": CONTRACT_VERSION, "kind": "nope", "backend": "local"})
    assert resp["ok"] is False
    assert "unknown task kind" in resp["error"]


def test_worker_contract_bad_version():
    resp = run_worker({"version": 99, "kind": "llm.run_trial", "backend": "local"})
    assert resp["ok"] is False


@pytest.mark.parametrize(
    "kind,key",
    [
        ("data.load", "rows"),
        ("data.synthetic", "variants"),
        ("grade", "missing"),
        ("prompt.search", "available"),
        ("ft.lora", "available"),
    ],
)
def test_worker_contract_handlers(kind, key, tmp_path):
    resp = run_worker(
        {
            "version": CONTRACT_VERSION,
            "kind": kind,
            "backend": "local",
            "data": {"source": "synthetic:3"},
            "task": {"id": "t", "expected": []},
            "stack": {"llm": {"planner": "reference:qwen-7b"}},
            "seed": 0,
            "workdir": str(tmp_path),
            "plan": {},
        }
    )
    assert resp["ok"] is True, resp
    assert key in resp["result"]["model_digests"]


def test_tune_backend_devbox(tmp_path, monkeypatch):
    """End-to-end: Rust core -> Python worker -> harness.tune.lock, offline."""
    monkeypatch.chdir(tmp_path)
    monkeypatch.setenv("HL_PYTHON", sys.executable)  # pin the interpreter the worker runs under

    manifest = f"""[project]
name = "worker-e2e"

[evals]
packs = ["{REPO / 'evals'}"]
seeds = 1

[candidates.llm.planner]
any_of = ["reference:qwen-7b"]

[candidates]
decision = ["none"]
harness = ["reference"]

[objective]
primary = "success-rate"
select = "min-cost"

[budget]
strategy = "grid"
max_trials = 1
"""
    (tmp_path / "harness.toml").write_text(manifest)

    import hlock

    out = json.loads(hlock.tune(backend="devbox", depth="probe", data="synthetic:1", max_trials=1))
    report = out["report"]
    assert report["backend"] == "devbox"
    assert (tmp_path / "harness.tune.lock").exists()
    assert report["best"]["trials"] == 1  # evidence.trials follows the winner's real trial count

    lf = hlock.Lockfile.load(str(tmp_path / "harness.tune.lock"))
    assert json.loads(lf.to_json())["kind"] == "tuned"


def test_worker_contract_prompt_search_digests():
    """prompt.search must produce a real selected prompt + before/after digests."""
    resp = run_worker(
        {
            "version": CONTRACT_VERSION,
            "kind": "prompt.search",
            "backend": "local",
            "task": {"id": "t", "prompt_text": "write a summary"},
            "seed": 0,
            "workdir": ".",
            "plan": {},
        }
    )
    assert resp["ok"] is True, resp
    md = resp["result"]["model_digests"]
    assert md["available"] is True
    assert md["best_prompt"] and md["best_prompt"].startswith("Be concise")  # chosen instruction
    assert str(md["before_digest"]).startswith("sha256:")
    assert str(md["after_digest"]).startswith("sha256:")
    assert md["after_digest"] != md["before_digest"]  # real (changed) prompt


def test_worker_contract_data_load_split():
    """data.load normalizes traces into a content-hashed train/val/test split."""
    resp = run_worker(
        {
            "version": CONTRACT_VERSION,
            "kind": "data.load",
            "backend": "local",
            "data": {"source": "user"},
            "seed": 0,
            "workdir": "/tmp/hl_dataload_test",
            "plan": {},
        }
    )
    # no trace files -> empty but well-formed split
    assert resp["ok"] is True, resp
    md = resp["result"]["model_digests"]
    assert "rows" in md and "digest" in md and "split" in md
    assert set(md["split"]) == {"train", "val", "test"}


def test_tune_depth_prompt_devbox(tmp_path, monkeypatch):
    """depth=prompt drives the data + prompt.search stages, writing real digests."""
    monkeypatch.chdir(tmp_path)
    monkeypatch.setenv("HL_PYTHON", sys.executable)
    monkeypatch.setenv("HL_LLM_BASE_URL", "")  # ensure deterministic reference path

    manifest = f"""[project]
name = "prompt-e2e"

[evals]
packs = ["{REPO / 'evals'}"]
seeds = 1

[candidates.llm.planner]
any_of = ["reference:qwen-7b"]

[candidates]
decision = ["none"]
harness = ["reference"]

[objective]
primary = "success-rate"
select = "min-cost"

[budget]
strategy = "grid"
max_trials = 2
"""
    (tmp_path / "harness.toml").write_text(manifest)

    import hlock

    json.loads(hlock.tune(backend="devbox", depth="prompt", data="synthetic:2", max_trials=2))
    d = json.loads(hlock.Lockfile.load(str(tmp_path / "harness.tune.lock")).to_json())
    applied = d["tune"]["prompt"]["applied"]
    assert applied and "_placeholder" not in applied[0]  # a real prompt search ran
    assert applied[0]["gain"]["success-rate"].startswith(("+", "-"))  # measured with and without it
    after = applied[0]["after"]
    assert after.startswith("sha256:") and after != applied[0]["before"]
    assert d["tune"]["data"]["mode"] == "synthetic"


# ---- trials against a served model (HL_LLM_BASE_URL) -------------------------------


class _Endpoint:
    """A local OpenAI-compatible endpoint that answers from the prompt, and records each
    request it gets."""

    def __init__(self, answer, usage=None):
        import http.server
        import threading

        self.requests = []
        outer = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_POST(self):
                body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                outer.requests.append(body)
                reply = {"choices": [{"message": {"content": answer(body["messages"][0]["content"])}}],
                         "usage": usage or {"prompt_tokens": 10, "completion_tokens": 5}}
                data = json.dumps(reply).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

            def log_message(self, *args):
                pass

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.url = f"http://127.0.0.1:{self.server.server_address[1]}/v1"
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def close(self):
        self.server.shutdown()


@pytest.fixture
def endpoint(monkeypatch):
    made = []

    def make(answer, usage=None):
        e = _Endpoint(answer, usage)
        made.append(e)
        monkeypatch.setenv("HL_LLM_BASE_URL", e.url)
        return e

    yield make
    for e in made:
        e.close()


def trial(planner: str, ws: Path, expected=None, prompt="Count the words in words.txt.") -> dict:
    return run_worker({
        "version": CONTRACT_VERSION,
        "kind": "llm.run_trial",
        "backend": "devbox",
        "stack": {"llm": {"planner": planner}},
        "task": {"id": "count_words", "prompt_text": prompt, "expected": expected or []},
        "seed": 0,
        "workdir": str(ws),
        "plan": {},
    })


def test_a_real_model_needs_an_endpoint(tmp_path, monkeypatch):
    monkeypatch.delenv("HL_LLM_BASE_URL", raising=False)
    resp = trial("openai:gpt-5", tmp_path)
    assert resp["ok"] is False
    assert resp["error"] == "the devbox backend needs a model endpoint for openai:gpt-5: set HL_LLM_BASE_URL (see Backends)"
    # the offline reference needs none, and says it was simulated
    resp = trial("reference:qwen-7b", tmp_path)
    assert resp["ok"] is True and resp["result"]["simulated"] is True


def test_the_reply_is_written_to_the_expected_artifact(tmp_path, monkeypatch, endpoint):
    (tmp_path / "words.txt").write_text("one two three\n")
    e = endpoint(lambda prompt: "```\n3\n```")
    monkeypatch.setenv("HL_LLM_MODEL", "served-model")
    resp = trial("openai:gpt-5", tmp_path, expected=["out/count.txt"])
    assert resp["ok"] is True, resp
    r = resp["result"]
    # the run completed; whether it's right is the graders' call
    assert r["success"] is True and r["simulated"] is False and r["steps"] == 1
    assert (tmp_path / "out/count.txt").read_text() == "3\n"  # the fence around the reply is dropped
    sent = e.requests[0]
    assert sent["model"] == "gpt-5"  # HL_LLM_MODEL stands in for reference candidates only
    assert "one two three" in sent["messages"][0]["content"]  # the workspace's files are in the prompt
    assert "Reply with only the full contents of `out/count.txt`" in sent["messages"][0]["content"]
    assert r["cost_usd"] is None and r["trace"][0]["cost_usd"] is None  # no price reported: unknown, not 0

    trial("reference:qwen-7b", tmp_path)  # no expected artifact: answer.md
    assert e.requests[1]["model"] == "served-model"
    assert (tmp_path / "answer.md").read_text() == "3\n"


def test_a_reported_cost_is_kept_and_a_failed_call_fails_the_trial(tmp_path, monkeypatch, endpoint):
    endpoint(lambda prompt: "3", usage={"prompt_tokens": 10, "completion_tokens": 5, "cost": 0.0012})
    assert trial("openai:gpt-5", tmp_path)["result"]["cost_usd"] == 0.0012
    monkeypatch.setenv("HL_LLM_BASE_URL", "http://127.0.0.1:9/v1")  # nothing listens there
    resp = trial("openai:gpt-5", tmp_path)
    assert resp["ok"] is False and resp["error"].startswith("the model endpoint failed for openai:gpt-5")


HELLO_ANSWERS = {"hello.txt": "Hello, world!", "count.txt": "9",
                 "greetings.md": "Hello, Ada!\nHello, Grace!\nHello, Linus!\n"}


def test_resolve_on_devbox_grades_what_the_model_wrote(tmp_path, monkeypatch, endpoint):
    """End to end: the model's replies land in trial workspaces that start from the
    fixtures, and the pack's graders decide each trial."""
    from click.testing import CliRunner

    from hlock.cli import main

    monkeypatch.chdir(tmp_path)
    monkeypatch.setenv("HL_PYTHON", sys.executable)
    seen = []

    def answer(prompt):
        seen.append(prompt)
        name = next((n for n in HELLO_ANSWERS if f"`{n}`, and nothing else" in prompt), None)
        return HELLO_ANSWERS.get(name, "?")

    endpoint(answer)
    assert CliRunner().invoke(main, ["evals", "init"]).exit_code == 0
    (tmp_path / "harness.toml").write_text(
        '[project]\nname = "devbox-e2e"\n\n[evals]\npacks = ["./evals"]\nseeds = 1\n\n'
        '[evals.splits]\nsearch = 1.0\n\n'
        '[candidates.llm.planner]\nany_of = ["openai:gpt-5"]\n\n'
        '[candidates]\ndecision = ["none"]\nharness = ["reference"]\n\n'
        '[objective]\nprimary = "success-rate"\nselect = "min-cost"\n\n[backend]\ntarget = "devbox"\n')
    r = CliRunner().invoke(main, ["resolve"])
    assert r.exit_code == 0, r.output
    lock = json.loads(CliRunner().invoke(main, ["show", "--json"]).output)
    assert lock["evidence"]["success-rate"]["mean"] == 1.0
    assert any("The quick brown fox" in p for p in seen)  # the fixtures reached the model
