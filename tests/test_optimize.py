"""Tune optimizers (specs/tune-optimizers.md): the bundle, the eval session, acceptance."""

import json
import shutil
import sys
from pathlib import Path

import pytest

import hlock
from hlock.optimize import BudgetExhausted, Session

TASKS = 9


class _Endpoint:
    """An OpenAI-compatible endpoint that answers OK only when the system prompt tells the
    model to check its result, and records each request."""

    def __init__(self, needle: str):
        import http.server
        import threading

        self.requests = []
        outer = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_POST(self):
                body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                outer.requests.append(body)
                system = " ".join(m["content"] for m in body["messages"] if m["role"] == "system")
                reply = {"choices": [{"message": {"content": "OK" if needle in system else "NO"}}],
                         "usage": {"prompt_tokens": 10, "completion_tokens": 1}}
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


@pytest.fixture
def project(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    monkeypatch.setenv("HL_PYTHON", sys.executable)
    pack = tmp_path / "evals"
    pack.mkdir()
    lines = ['[pack]\nname = "say-ok"\nversion = "0.1.0"\n']
    for i in range(TASKS):
        (pack / f"say_{i}.md").write_text("Answer the question.\n")
        lines.append(f'[[task]]\nid = "say_{i}"\nprompt = "say_{i}.md"\nexpected = {{ artifacts = ["answer.md"] }}\n'
                     'graders = [{ type = "artifact", path = "answer.md", contains = "OK" }]\n')
    (pack / "pack.toml").write_text("\n".join(lines))
    (tmp_path / "AGENTS.md").write_text("Be careful.\n")
    (tmp_path / "harness.toml").write_text("""[project]
name = "opt"

[evals]
packs = ["./evals"]
seeds = 1

[candidates.llm.planner]
any_of = ["reference:qwen-7b"]

[candidates]
decision = ["none"]
harness = ["reference"]

[tune]
trials = 1
alpha = 0.2
budget = { rollouts = 200 }
""")
    endpoint = _Endpoint("Check your result")
    monkeypatch.setenv("HL_LLM_BASE_URL", endpoint.url)
    yield tmp_path, endpoint
    endpoint.server.shutdown()


def tune_lock(path: Path) -> dict:
    return json.loads(hlock.Lockfile.load(str(path / "harness.tune.lock")).to_json())


def test_the_built_in_optimizer_finds_an_edit_and_hlock_accepts_it(project):
    path, endpoint = project
    report = json.loads(hlock.tune(backend="local", depth="prompt", max_trials=2))["report"]
    assert {"stage": "optimize", "ran": True, "detail": "1 edit accepted"} in report["stages"]
    d = tune_lock(path)
    acc = d["tune"]["acceptance"]
    assert acc["accepted"] is True and acc["test"]["split"] == "holdout"
    assert acc["test"]["base"] == 0.0 and acc["test"]["candidate"] == 1.0
    edit = d["tune"]["edit"][0]
    assert (edit["component"], edit["kind"], edit["op"]) == ("instructions/AGENTS.md", "instructions", "add")
    assert "built-in guidance" in edit["rationale"]
    tuned = (path / edit["path"]).read_text()
    assert tuned.startswith("Be careful.\n") and "Check your result" in tuned
    entry = next(i for i in d["instructions"] if i["name"] == "AGENTS.md")
    assert (entry["source"], entry["vendored"]) == ("tune", edit["path"])
    opt = d["tune"]["optimizer"]
    assert opt["name"] == "hlock" and opt["evaluator"] == "hlock" and 0 < opt["usage"]["rollouts"] <= 200
    # trials ran with the instruction files as the system prompt
    systems = [m["content"] for r in endpoint.requests for m in r["messages"] if m["role"] == "system"]
    assert any(s.startswith("Be careful.") for s in systems)
    # and the tuned lock's own evidence was measured with the edit
    assert d["evidence"]["success-rate"]["mean"] == 1.0


def test_the_session_keeps_the_test_split_and_the_budget(project):
    path, _ = project
    hlock.tune(backend="local", depth="prompt", max_trials=2)
    sdir = path / ".hlock" / "tune" / tune_lock(path)["tune"]["optimizer"]["session"]
    session = Session(sdir)
    seed = sdir / "seed"
    with pytest.raises(ValueError, match="test split is hlock's"):
        session.evaluate(seed, "test")
    s = json.loads((sdir / "session.json").read_text())
    train = s["train"]
    with pytest.raises(ValueError, match="isn't in the dev split"):
        session.evaluate(seed, "dev", tasks=[t for t in train if t not in s["dev"]][:1])
    out = session.evaluate(seed, "train", tasks=train[:1])
    assert out["results"][0]["cached"] is True  # the noise band already ran the seed
    # a candidate the bundle doesn't allow is refused before any trial
    bad = path / "bad"
    shutil.copytree(seed, bad)
    (bad / "run.sh").write_text("echo hi\n")
    with pytest.raises(ValueError, match="isn't part of a bundle"):
        session.evaluate(bad, "train")
    # a spent budget stops new trials
    s["budget"]["rollouts"] = s["used"]["rollouts"]
    (sdir / "session.json").write_text(json.dumps(s))
    fresh = path / "fresh"
    shutil.copytree(seed, fresh)
    (fresh / "instructions" / "AGENTS.md").write_text("Be careful.\nSomething new.\n")
    with pytest.raises(BudgetExhausted):
        session.evaluate(fresh, "train")


OPTIMIZER = '''
import json, shutil
from pathlib import Path
from hlock.optimizers import Optimizer
from hlock.optimize import Session


class Mine(Optimizer):
    name = "mine"

    def optimize(self, request):
        opt = request["optimize"]
        ws = Path(request["workdir"])
        cand = ws / "best"
        shutil.copytree(opt["seed"], cand)
        f = cand / "instructions" / "AGENTS.md"
        f.write_text(f.read_text() + "Check your result before you finish. Mention say_3.\\n")
        Session(opt["session"]).evaluate(cand, "train")
        (ws / "ledger.jsonl").write_text(json.dumps({"component": "instructions/AGENTS.md", "op": "modify",
                                                     "rationale": "mine"}) + "\\n")
        return {"candidate": str(cand), "ledger": str(ws / "ledger.jsonl"),
                "optimizer": {"name": "mine", "version": "1.0"}}
'''


def test_an_optimizer_whose_edit_copies_the_pack_is_dropped(project, monkeypatch):
    path, _ = project
    (path / "mine_opt.py").write_text(OPTIMIZER)
    monkeypatch.setenv("PYTHONPATH", str(path))
    with open(path / "harness.toml", "a") as f:
        f.write('\n[optimizers.mine]\npython = "mine_opt:Mine"\n')
    report = json.loads(hlock.tune(backend="local", depth="prompt", max_trials=2, optimizer="mine"))["report"]
    assert any("dropped the edit to instructions/AGENTS.md" in n and "say_3" in n for n in report["notes"])
    d = tune_lock(path)
    assert d["tune"]["optimizer"]["name"] == "mine" and d["tune"]["optimizer"]["version"] == "1.0"
    assert d["tune"]["acceptance"]["accepted"] is False
    assert "edit" not in d["tune"]
