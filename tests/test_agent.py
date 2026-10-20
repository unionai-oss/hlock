"""hlock's built-in agent, against a fake model server that speaks the OpenAI chat
completions and Anthropic Messages APIs and replies from a script."""

import json
import os
import re
import stat
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest
from click.testing import CliRunner

import harness_lock
from harness_lock import Lockfile, agent_run
from harness_lock.cli import main


def invoke(*args):
    return CliRunner().invoke(main, list(args))


class FakeModel:
    """Replies to each request with the next scripted turn: a list of tool calls
    `(name, args)`, or a string (the model's final words)."""

    def __init__(self, script):
        self.script = list(script)
        self.requests = []
        model = self

        class Handler(BaseHTTPRequestHandler):
            def do_POST(self):
                body = json.loads(self.rfile.read(int(self.headers["content-length"])))
                model.requests.append({"path": self.path, "headers": dict(self.headers), "body": body})
                if self.headers.get("authorization") == "Bearer wrong":
                    return self.reply(401, {"error": {"message": "invalid api key"}})
                turn = model.script.pop(0) if model.script else "done"
                self.reply(200, model.anthropic(turn) if self.path.endswith("/messages") else model.openai(turn))

            def reply(self, code, payload):
                data = json.dumps(payload).encode()
                self.send_response(code)
                self.send_header("content-type", "application/json")
                self.send_header("content-length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

            def log_message(self, *a):
                pass

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        self.url = f"http://127.0.0.1:{self.server.server_port}/v1"

    @staticmethod
    def openai(turn):
        if isinstance(turn, str):
            msg = {"role": "assistant", "content": turn}
        else:
            msg = {"role": "assistant", "content": None, "tool_calls": [
                {"id": f"c{i}", "type": "function", "function": {"name": n, "arguments": json.dumps(a)}}
                for i, (n, a) in enumerate(turn)]}
        return {"choices": [{"message": msg}], "usage": {"prompt_tokens": 10, "completion_tokens": 5}}

    @staticmethod
    def anthropic(turn):
        if isinstance(turn, str):
            content = [{"type": "text", "text": turn}]
        else:
            content = [{"type": "tool_use", "id": f"t{i}", "name": n, "input": a} for i, (n, a) in enumerate(turn)]
        return {"content": content, "usage": {"input_tokens": 10, "output_tokens": 5}}


@pytest.fixture
def project(tmp_path, monkeypatch):
    for key in list(os.environ):
        if key.startswith(("CLAUDE", "PI_", "HLOCK_", "XDG_", "OLLAMA_")) or key.endswith(("_API_KEY", "_TOKEN")):
            monkeypatch.delenv(key)
    home, proj, bindir = tmp_path / "home", tmp_path / "project", tmp_path / "bin"
    for d in (home, proj, bindir):
        d.mkdir()
    # `hlock` for the agent's commands: this checkout's CLI
    hl = bindir / "hlock"
    hl.write_text(f'#!/bin/sh\nexec "{sys.executable}" -m harness_lock.cli "$@"\n')
    hl.chmod(hl.stat().st_mode | stat.S_IXUSR)
    monkeypatch.setenv("HOME", str(home))
    monkeypatch.setenv("PATH", f"{bindir}{os.pathsep}/usr/bin{os.pathsep}/bin")
    monkeypatch.setenv("NO_COLOR", "1")
    monkeypatch.setenv("HL_PYTHON", sys.executable)
    monkeypatch.chdir(proj)
    return proj


NOTES_JOB = '\n[jobs.notes]\ndescription = "Write notes"\nprompt = "Write NOTES.md"\nverify = "test -s NOTES.md"\n'


def setup_builtin(project, fake, monkeypatch, provider="openai"):
    monkeypatch.setenv(f"{provider.upper()}_API_KEY", "sk-test-0123456789abcdef")
    r = invoke("meta", "init", "--provider", provider, "--base-url", fake.url)
    assert r.exit_code == 0, r.output
    mm = project / "harness.meta.toml"
    mm.write_text(mm.read_text() + NOTES_JOB)


def test_builtin_is_the_default(project, monkeypatch):
    monkeypatch.setenv("ANTHROPIC_API_KEY", "sk-ant-0123456789abcdef")
    r = invoke("meta", "init")
    assert r.exit_code == 0, r.output
    assert "built-in agent  ships with hlock; nothing to install" in r.output
    assert "install" not in r.output.split("pinned")[0].lower().replace("nothing to install", "")
    lock = json.loads(Lockfile.load("harness.meta.lock").to_json())
    h = lock["harness"]
    assert (h["name"], h["version"], h["source"]) == ("builtin", harness_lock.__version__, f"hlock@{harness_lock.__version__}")
    assert [(x["provider"], x["model"], x["params"]) for x in lock["llm"]] == [
        ("anthropic", "claude-sonnet-5-5", {"key-env": "ANTHROPIC_API_KEY"})]
    assert lock["policy"]["tool-allowlist"] == ["@hlock", "git log", "git show", "git diff", "git status"]
    assert 'harness = "builtin"' in (project / "harness.meta.toml").read_text()
    for p in project.rglob("*"):
        if p.is_file():
            assert "sk-ant" not in p.read_text(errors="ignore")
    assert invoke("meta", "check").exit_code == 0
    assert "nothing to install" in invoke("meta", "sync").output
    assert invoke("meta", "launch").exit_code == 1


def test_builtin_runs_a_job_openai(project, monkeypatch):
    fake = FakeModel([
        [("list_dir", {"path": "."}), ("write_file", {"path": "NOTES.md", "content": "# Notes\n"})],
        [("run", {"command": "hlock meta verify notes"})],
        "Wrote NOTES.md and verified it.",
    ])
    setup_builtin(project, fake, monkeypatch)
    # a custom job's check runs a command from harness.meta.toml, so @hlock leaves it out; allow it here
    mm = project / "harness.meta.toml"
    mm.write_text(mm.read_text().replace('allow = ["@hlock",', 'allow = ["@hlock", "hlock meta verify notes",'))
    assert invoke("meta", "pin").exit_code == 0
    r = invoke("meta", "run", "notes", "--headless")
    assert r.exit_code == 0, r.output
    assert "✓ verified" in r.output and "→ write file NOTES.md" in r.output and "→ $ hlock meta verify notes" in r.output
    assert "Wrote NOTES.md and verified it." in r.output and "built-in agent finished (3 steps, 45 tokens)" in r.output
    assert (project / "NOTES.md").read_text() == "# Notes\n"
    first, second, third = fake.requests
    assert first["path"] == "/v1/chat/completions" and first["headers"]["authorization"] == "Bearer sk-test-0123456789abcdef"
    assert first["body"]["model"] == "gpt-5.5" and {t["function"]["name"] for t in first["body"]["tools"]} >= {"run", "write_file"}
    assert "Write NOTES.md" in first["body"]["messages"][1]["content"]
    tool_msgs = [m for m in second["body"]["messages"] if m["role"] == "tool"]
    assert [m["tool_call_id"] for m in tool_msgs] == ["c0", "c1"] and "wrote NOTES.md" in tool_msgs[1]["content"]
    verify_result = [m for m in third["body"]["messages"] if m["role"] == "tool"][-1]["content"]
    assert verify_result.startswith("exit code 0") and "verified" in verify_result


def test_builtin_headless_refuses_unlisted_commands(project, monkeypatch):
    (project / "keep.txt").write_text("keep me\n")
    fake = FakeModel([[("run", {"command": "rm keep.txt"})], [("run", {"command": "git log; rm keep.txt"})], "Couldn't."])
    setup_builtin(project, fake, monkeypatch)
    r = invoke("meta", "run", "notes", "--headless")
    assert r.exit_code == 1  # NOTES.md wasn't written
    assert r.output.count("✗ refused") == 2 and (project / "keep.txt").exists()
    refusal = next(m for m in fake.requests[1]["body"]["messages"] if m["role"] == "tool")["content"]
    assert "refused" in refusal and "unattended" in refusal


def test_builtin_writes_stay_in_the_project(project, tmp_path, monkeypatch):
    fake = FakeModel([[("write_file", {"path": "../outside.txt", "content": "x"})], "ok"])
    setup_builtin(project, fake, monkeypatch)
    invoke("meta", "run", "notes", "--headless")
    assert not (tmp_path / "outside.txt").exists()
    result = next(m for m in fake.requests[1]["body"]["messages"] if m["role"] == "tool")["content"]
    assert "outside the project" in result


def test_builtin_anthropic(project, monkeypatch):
    fake = FakeModel([[("write_file", {"path": "NOTES.md", "content": "hi\n"})], "Done."])
    setup_builtin(project, fake, monkeypatch, provider="anthropic")
    r = invoke("meta", "run", "notes", "--headless")
    assert r.exit_code == 0, r.output
    first, second = fake.requests
    assert first["path"] == "/v1/messages" and first["headers"]["x-api-key"] == "sk-test-0123456789abcdef"
    assert first["headers"]["anthropic-version"] == "2023-06-01" and first["body"]["model"] == "claude-sonnet-5-5"
    assert "built-in agent of hlock" in first["body"]["system"] and first["body"]["tools"][0]["input_schema"]
    blocks = second["body"]["messages"][-1]["content"]
    assert blocks[0]["type"] == "tool_result" and blocks[0]["tool_use_id"] == "t0"


def test_builtin_interactive_asks(project, monkeypatch):
    (project / "keep.txt").write_text("keep me\n")
    fake = FakeModel([[("run", {"command": "rm keep.txt"})], [("run", {"command": "touch made.txt"})],
                      [("run", {"command": "touch again.txt"})], "ok"])
    setup_builtin(project, fake, monkeypatch)
    asked, said = [], []
    answers = iter(["n", "a"])

    def ask(cmd):
        asked.append(cmd)
        return next(answers)

    out = json.loads(agent_run("go", meta_lock="harness.meta.lock", interactive=True,
                               say=lambda k, t: said.append((k, t)) or True, ask=ask, project=str(project)))
    assert out["status"] == "done" and out["steps"] == 4
    assert asked == ["rm keep.txt", "touch made.txt"]  # "always" covers the second touch
    assert (project / "keep.txt").exists() and (project / "made.txt").exists() and (project / "again.txt").exists()
    assert ("denied", "rm keep.txt") in said


def test_builtin_stops_when_asked(project, monkeypatch):
    fake = FakeModel([[("list_dir", {})]] * 5)
    setup_builtin(project, fake, monkeypatch)
    out = json.loads(agent_run("go", meta_lock="harness.meta.lock", say=lambda k, t: k != "tool", project=str(project)))
    assert out["status"] == "stopped" and len(fake.requests) == 1


def test_builtin_raises_what_a_callback_raised(project, monkeypatch):
    fake = FakeModel([[("list_dir", {})]] * 5)
    setup_builtin(project, fake, monkeypatch)

    def say(kind, text):
        if kind == "tool":
            raise RuntimeError("from say")
        return True

    with pytest.raises(RuntimeError, match="from say"):
        agent_run("go", meta_lock="harness.meta.lock", say=say, project=str(project))
    assert len(fake.requests) == 1, "the run stopped at the exception"


def test_builtin_raises_ctrl_c_from_ask(project, monkeypatch):
    fake = FakeModel([[("run", {"command": "rm x"})], "ok"])
    setup_builtin(project, fake, monkeypatch)

    def ask(cmd):
        raise KeyboardInterrupt

    with pytest.raises(KeyboardInterrupt):
        agent_run("go", meta_lock="harness.meta.lock", interactive=True, ask=ask, project=str(project))


def test_builtin_reports_api_errors(project, monkeypatch):
    fake = FakeModel([])
    setup_builtin(project, fake, monkeypatch)
    monkeypatch.setenv("OPENAI_API_KEY", "wrong")
    r = invoke("meta", "run", "notes", "--headless")
    assert r.exit_code == 1 and "returned 401: invalid api key" in r.output and "didn't finish" in r.output


def test_builtin_lock_pins_what_it_may_do(project, monkeypatch):
    fake = FakeModel([])
    setup_builtin(project, fake, monkeypatch)
    mm = project / "harness.meta.toml"
    mm.write_text(re.sub(r"^allow = .*$", 'allow = ["hlock", "brew install"]\nmax-steps = 12', mm.read_text(), flags=re.MULTILINE))
    assert invoke("meta", "pin").exit_code == 0
    lock = json.loads(Lockfile.load("harness.meta.lock").to_json())
    assert lock["policy"]["tool-allowlist"] == ["hlock", "brew install"] and lock["policy"]["max-steps"] == 12
    mm.write_text(mm.read_text().replace('"brew install"', '"npm install"'))
    r = invoke("meta", "check")
    assert r.exit_code == 1 and "~ agent allow: hlock, brew install → hlock, npm install" in r.output
    assert invoke("meta", "pin").exit_code == 0 and invoke("meta", "check").exit_code == 0
    # a lock from another hlock version is drift, and runs need --allow-version-mismatch
    lp = project / "harness.meta.lock"
    lp.write_text(lp.read_text().replace(f'version = "{harness_lock.__version__}"', 'version = "0.0.1"'))
    assert "the lock pins hlock 0.0.1" in invoke("meta", "check").output
    r = invoke("meta", "run", "notes", "--headless")
    assert r.exit_code == 1 and "--allow-version-mismatch" in r.output


def test_builtin_local_model_needs_no_key(project, monkeypatch):
    fake = FakeModel([[("write_file", {"path": "NOTES.md", "content": "local\n"})], "ok"])
    (project.parent / "bin" / "ollama").write_text("#!/bin/sh\necho 'ollama version is 0.12.0'\n")
    (project.parent / "bin" / "ollama").chmod(0o755)
    r = invoke("meta", "init", "--local", "--model", "qwen3:8b", "--base-url", fake.url)
    assert r.exit_code == 0, r.output
    (project / "harness.meta.toml").write_text((project / "harness.meta.toml").read_text() + NOTES_JOB)
    lock = json.loads(Lockfile.load("harness.meta.lock").to_json())
    assert lock["llm"][0]["runtime"] == "ollama" and lock["llm"][0]["endpoint"] == fake.url
    r = invoke("meta", "run", "notes", "--headless")
    assert r.exit_code == 0, r.output
    assert "authorization" not in fake.requests[0]["headers"]
