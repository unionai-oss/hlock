"""Pluggable backends (specs/backends.md): the `hlock.backends` API, the worker finding a
backend by name, and a backend defined in `[backends.<name>]` running resolve and tune."""

import json
import os
import sys
from pathlib import Path

import pytest
from click.testing import CliRunner
from test_worker import run_worker

from hlock.backends import KINDS, Backend, BackendError, load, run_local
from hlock.backends.local import LocalBackend
from hlock.cli import main
from hlock.tasks.worker import CONTRACT_VERSION

# A backend that runs only trials, on hlock's stock handler, and says where it ran them.
TRIALS_ONLY = '''
from hlock.backends import Backend, run_local

class TrialsOnly(Backend):
    name = "mine"
    kinds = ("llm.run_trial",)
    about = "trials only"

    def run(self, request):
        result = run_local(request)
        result["model_digests"]["ran_on"] = request["backend_options"].get("where")
        return result

instance = TrialsOnly()
'''


def invoke(*args):
    return CliRunner().invoke(main, list(args))


def flat(text: str) -> str:
    return " ".join(text.replace("│", " ").split())


def test_load_finds_built_ins_objects_and_says_what_is_missing():
    assert isinstance(load("local"), LocalBackend)
    assert load("x", "hlock.backends.local:LocalBackend").name == "local"
    with pytest.raises(BackendError, match="no backend named 'nowhere': not built in, not in \\[backends\\]"):
        load("nowhere")
    with pytest.raises(BackendError, match="can't load backend 'x' from nope.mod:Thing"):
        load("x", "nope.mod:Thing")
    with pytest.raises(BackendError, match="isn't a backend: it has no run"):
        load("x", "json:dumps")


def test_the_base_class_describes_itself_and_run_local_runs_the_stock_handlers():
    assert Backend().describe({"backend": "given"}) == {"name": "given", "kinds": list(KINDS), "model_endpoint": True, "about": ""}
    req = {"version": CONTRACT_VERSION, "kind": "data.synthetic", "backend": "local", "plan": {"data": "synthetic:2"},
           "workdir": "."}
    assert run_local(req) == LocalBackend().run(req)
    with pytest.raises(BackendError, match="unknown task kind"):
        run_local({"kind": "nope"})


def test_the_worker_describes_and_runs_the_backend_the_request_names(tmp_path, monkeypatch):
    (tmp_path / "mine_backend.py").write_text(TRIALS_ONLY)
    monkeypatch.syspath_prepend(str(tmp_path))
    monkeypatch.setenv("PYTHONPATH", os.pathsep.join([str(tmp_path), os.environ.get("PYTHONPATH", "")]))
    resp = run_worker({"version": CONTRACT_VERSION, "kind": "backend.describe", "backend": "local"})
    assert resp["ok"] and resp["result"]["kinds"] == list(KINDS) and resp["result"]["model_endpoint"] is True
    resp = run_worker({"version": CONTRACT_VERSION, "kind": "backend.describe", "backend": "mine",
                       "backend_object": "mine_backend:instance"})
    assert resp["result"] == {"name": "mine", "kinds": ["llm.run_trial"], "model_endpoint": True, "about": "trials only"}
    resp = run_worker({"version": CONTRACT_VERSION, "kind": "backend.describe", "backend": "nowhere"})
    assert not resp["ok"] and resp["error"].startswith("no backend named 'nowhere'")


@pytest.fixture
def project(tmp_path, monkeypatch):
    for key in list(os.environ):
        if key.startswith(("HLOCK_", "HL_LLM")):
            monkeypatch.delenv(key)
    monkeypatch.chdir(tmp_path)
    monkeypatch.setenv("NO_COLOR", "1")
    monkeypatch.setenv("HL_PYTHON", sys.executable)
    (tmp_path / "mine_backend.py").write_text(TRIALS_ONLY)
    monkeypatch.setenv("PYTHONPATH", os.pathsep.join([str(tmp_path), os.environ.get("PYTHONPATH", "")]))
    assert invoke("evals", "init").exit_code == 0
    assert invoke("init", "--pack", "./evals").exit_code == 0
    manifest = tmp_path / "harness.toml"
    manifest.write_text(manifest.read_text().replace('target = "offline"', 'target = "mine"')
                        + '\n[backends.mine]\npython = "mine_backend:instance"\noptions = { where = "here" }\n')
    return tmp_path


def test_a_python_backend_from_the_manifest_runs_resolve_and_tune(project):
    r = invoke("resolve")
    assert r.exit_code == 0, r.output
    lock = json.loads(invoke("show", "--json").output)
    assert lock["provenance"]["backend"] == "mine"

    r = invoke("tune", "--depth", "prompt")
    assert r.exit_code == 0, r.output
    out = flat(r.output)
    assert "data ✗ the mine backend doesn't run data.load" in out
    assert "optimize ✗ the mine backend doesn't run harness.optimize" in out

    r = invoke("backend", "status")
    assert r.exit_code == 0, r.output
    out = flat(r.output)
    assert "backend mine (from harness.toml)" in out and "Python object mine_backend:instance" in out
    assert "runs llm.run_trial" in out and "options where=here" in out
    assert "mine harness.toml: Python object mine_backend:instance" in flat(invoke("backend", "list").output)


def test_old_names_still_work_and_a_bad_backends_table_stops_resolve(project):
    r = invoke("backend", "status", "--backend", "devbox")
    assert r.exit_code == 0 and "backend local (from --backend; 'devbox' is its old name)" in flat(r.output)
    manifest = project / "harness.toml"
    manifest.write_text(manifest.read_text().replace('python = "mine_backend:instance"', 'pyhton = "mine_backend:instance"'))
    r = invoke("resolve")
    assert r.exit_code == 1
    assert "unknown key `pyhton` in [backends.mine] (did you mean `python`?)" in flat(r.output)
    assert not (project / ".hlock/runs").exists()


SYNTH = {"version": CONTRACT_VERSION, "kind": "data.synthetic", "backend": "flyte", "plan": {"data": "synthetic:2"}, "workdir": "."}


def test_flyte_runs_in_process_without_the_sdk(monkeypatch, capsys):
    import hlock.backends.flyte as fb

    monkeypatch.setattr(fb, "flyte", None)
    assert fb.FlyteBackend().run(dict(SYNTH)) == run_local(dict(SYNTH))
    assert "the Flyte SDK isn't installed" in capsys.readouterr().err


sent = []  # the requests the fake Flyte SDK's runs were given


def fake_flyte(calls, fail=None):
    """A stand-in for the Flyte SDK whose runs execute the task body here, as a pod would."""
    import hlock.backends.flyte as fb

    class Run:
        url = "https://flyte.example/runs/r1"

        def __init__(self, request):
            self.request = request

        def wait(self):
            calls.append(("wait",))

        def raise_for_status(self):
            if fail:
                raise RuntimeError(fail)

        def outputs(self):
            return (fb._run_in_pod(dict(self.request)),)

    class Runner:
        def __init__(self, ctx):
            self.ctx = ctx

        def run(self, task, request):
            calls.append(("run", task, self.ctx))
            sent.append(dict(request))
            return Run(request)

    class FakeFlyte:
        @staticmethod
        def init_from_config(config=None, root_dir=None, **scope):
            assert (root_dir / "hlock" / "backends" / "flyte.py").is_file()
            calls.append(("init", config, scope))

        @staticmethod
        def with_runcontext(**ctx):
            return Runner(ctx)

    return FakeFlyte


def test_flyte_lifts_each_item_to_a_task_from_the_image_and_records_the_run(monkeypatch, tmp_path):
    import hlock.backends.flyte as fb

    calls = []
    monkeypatch.setattr(fb, "flyte", fake_flyte(calls))
    monkeypatch.setattr(fb, "run_item", "cpu-task", raising=False)  # defined only with the SDK
    monkeypatch.setattr(fb, "_gpu_task", lambda gpu: f"gpu-task:{gpu}")
    req = dict(SYNTH, workdir=str(tmp_path), backend_options={"project": "agents", "domain": "development", "config": "dev.yaml"})
    result = fb.FlyteBackend().run(req)
    assert result["model_digests"]["run"] == "https://flyte.example/runs/r1" and result["model_digests"]["backend"] == "flyte"
    assert calls[0] == ("init", "dev.yaml", {"project": "agents", "domain": "development"})
    run = calls[1]
    assert run[1] == "cpu-task" and run[2]["copy_style"] == "none" and run[2]["project"] == "agents"
    assert run[2]["version"] == fb._task_version()
    # fine-tuning goes to the GPU task, on the accelerator the options name
    fb.FlyteBackend().run(dict(SYNTH, kind="ft.lora", workdir=str(tmp_path), backend_options={"gpu": "H100:1"}))
    assert any(c[0] == "run" and c[1] == "gpu-task:H100:1" for c in calls)


def test_flyte_sends_the_workspace_and_brings_back_what_the_task_wrote(monkeypatch, tmp_path):
    import hlock.backends.flyte as fb

    def handler(request):
        ws = Path(request["workdir"])
        assert (ws / "in.txt").read_text() == "input" and ws != tmp_path
        (ws / "out" / "answer.md").parent.mkdir()
        (ws / "out" / "answer.md").write_text("written in the pod")
        return {"success": True, "model_digests": {}}

    monkeypatch.setattr(fb, "flyte", fake_flyte([]))
    monkeypatch.setattr(fb, "run_item", "cpu-task", raising=False)
    monkeypatch.setattr(fb, "run_local", handler)
    (tmp_path / "in.txt").write_text("input")
    (tmp_path / "_al_request.json").write_text("{}")
    result = fb.FlyteBackend().run(dict(SYNTH, kind="llm.run_trial", workdir=str(tmp_path)))
    assert result["success"] and "files" not in result
    assert (tmp_path / "out" / "answer.md").read_text() == "written in the pod"
    # a returned file can't land outside the workspace
    with pytest.raises(BackendError, match="outside its workspace"):
        fb._unpack({"../escape.txt": "eA=="}, tmp_path)


def test_a_failed_flyte_run_fails_the_item_and_isnt_rerun_here(monkeypatch, tmp_path):
    import hlock.backends.flyte as fb

    monkeypatch.setattr(fb, "flyte", fake_flyte([], fail="OOMKilled"))
    monkeypatch.setattr(fb, "run_item", "cpu-task", raising=False)
    with pytest.raises(BackendError, match="the Flyte run https://flyte.example/runs/r1 failed: OOMKilled"):
        fb.FlyteBackend().run(dict(SYNTH, workdir=str(tmp_path)))


def test_a_local_wheel_goes_into_the_image_for_its_platform(monkeypatch, tmp_path):
    import hlock.backends.flyte as fb

    wheel = tmp_path / "hlock-0.5.0-cp39-abi3-manylinux_2_17_aarch64.manylinux2014_aarch64.whl"
    wheel.write_bytes(b"wheel")
    monkeypatch.setenv("HLOCK_FLYTE_HLOCK", str(wheel))
    v1 = fb._task_version()
    wheel.write_bytes(b"another build")
    assert fb._task_version() != v1  # a new build registers new tasks
    monkeypatch.setenv("HLOCK_FLYTE_HLOCK", "hlock==0.5.0")
    assert fb._task_version().startswith("0.5.0-")


def test_flyte_tasks_get_the_endpoint_but_the_key_only_as_a_secret(monkeypatch, tmp_path):
    import hlock.backends.flyte as fb

    class Task:
        def override(self, secrets):
            return ("secret-task", secrets.key, secrets.as_env_var)

    calls = []
    fake = fake_flyte(calls)
    fake.Secret = lambda key, as_env_var: type("S", (), {"key": key, "as_env_var": as_env_var})()
    monkeypatch.setattr(fb, "flyte", fake)
    monkeypatch.setattr(fb, "run_item", Task(), raising=False)
    monkeypatch.setenv("HL_LLM_BASE_URL", "http://models.internal/v1")
    monkeypatch.setenv("HL_LLM_API_KEY", "sk-test")
    with pytest.raises(BackendError, match="store it as a Flyte secret"):
        fb.FlyteBackend().run(dict(SYNTH, workdir=str(tmp_path)))
    fb.FlyteBackend().run(dict(SYNTH, workdir=str(tmp_path), backend_options={"api_key_secret": "llm-key"}))
    run = [c for c in calls if c[0] == "run"][-1]
    assert run[1] == ("secret-task", "llm-key", "HL_LLM_API_KEY")
    assert "sk-test" not in str(run) and "sk-test" not in str(sent)
    # the endpoint (not the key) goes in the inputs, so the task cache keys on it
    assert sent[-1]["env"] == {"HL_LLM_BASE_URL": "http://models.internal/v1"} and sent[-1]["hlock"] == fb._task_version()
