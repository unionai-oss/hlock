"""Deciders wired into Gemini CLI, Cursor, Copilot, Amp, OpenCode and Pi: `pin` locks them,
`sync` writes the hooks or plugins, and the plugins (run in Node with a mock of each
harness's API) call `hlock decide hook` and act on its answer."""

import json
import os
import shutil
import stat
import subprocess
import sys
from pathlib import Path

import pytest
from click.testing import CliRunner
from fake_models import Fake, systemone

from harness_lock import Lockfile
from harness_lock.cli import main

NODE = shutil.which("node")
HLOCK = str(Path(sys.executable).parent / "hlock")


def invoke(*args):
    return CliRunner().invoke(main, list(args))


def executable(path: Path, body: str) -> None:
    path.write_text(body)
    path.chmod(path.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)


@pytest.fixture
def project(tmp_path, monkeypatch):
    for key in list(os.environ):
        if key.startswith(("CLAUDE", "CODEX_", "PI_", "OPENCODE", "HERMES_", "CURSOR_", "COPILOT_", "GEMINI_", "AMP_",
                           "HLOCK_", "XDG_", "OLLAMA_", "TYPESAFE_")):
            monkeypatch.delenv(key)
    home, proj, bindir = tmp_path / "home", tmp_path / "project", tmp_path / "bin"
    for d in (home, proj, bindir):
        d.mkdir()
    for name, version in {"gemini": "0.30.0", "cursor-agent": "2026.10.01", "copilot": "1.0.94", "amp": "0.0.1760000000",
                          "opencode": "2.0.30", "pi": "1.3.0"}.items():
        executable(bindir / name, f"#!/bin/sh\necho {version}\n")
    monkeypatch.setenv("HOME", str(home))
    monkeypatch.setenv("PATH", f"{bindir}{os.pathsep}{Path(sys.executable).parent}{os.pathsep}/usr/bin{os.pathsep}/bin")
    monkeypatch.setenv("NO_COLOR", "1")
    monkeypatch.setenv("HLOCK_JUDGE_AUTO", "0")
    monkeypatch.setenv("HLOCK_BIN", HLOCK)
    monkeypatch.chdir(proj)
    return {"home": home, "project": proj}


@pytest.fixture
def deny():
    f = Fake(lambda path, body: systemone(body, noul=0.1, choice="deny", p=0.97))
    yield f
    f.close()


def write_manifest(url: str, uses=("approve",)) -> None:
    Path("harness.toml").write_text("".join(
        f'[deciders.{u}]\nmodel = "system-one:kev-4b"\nbase-url = "{url}"\n\n' for u in uses))


# harness, uses, the file it reads hooks from (its name), what to look for in it
CASES = [
    ("gemini-cli", ("approve", "stop", "tool"), "settings.json", "--format gemini --event BeforeToolSelection"),
    ("cursor-agent", ("approve",), "hooks.json", "--format cursor --event preToolUse"),
    ("copilot-cli", ("approve",), "hlock.json", "--format copilot --event preToolUse"),
    ("amp", ("approve", "stop"), "hlock-deciders.js", 'USES = ["approve","stop"]'),
    ("opencode", ("approve",), "hlock-deciders.js", 'USES = ["approve"]'),
    ("pi", ("approve",), "hlock-deciders.ts", 'USES = ["approve"]'),
]


def wired_file(plan: dict) -> Path:
    step = next(s for s in plan["steps"] if s["id"].startswith("decider:"))
    return Path(step["action"]["path"])


@pytest.mark.parametrize("harness, uses, name, needle", CASES, ids=[c[0] for c in CASES])
def test_sync_wires_deciders_into(project, deny, harness, uses, name, needle):
    write_manifest(deny.url, uses)
    r = invoke("pin", "--harness", harness)
    assert r.exit_code == 0, r.output
    lock = json.loads(Lockfile.load("harness.lock").to_json())
    assert sorted(d["use"] for d in lock["decider"]) == sorted(uses)
    plan = json.loads(invoke("sync", "--plan", "--json").output)
    path = wired_file(plan)
    assert path.name == name
    if harness == "copilot-cli":
        assert path == Path(".github/hooks/hlock.json").resolve(), "Copilot reads hooks only from the repository"
    r = invoke("sync", "--yes")
    assert r.exit_code == 0, r.output
    assert path.is_file(), path
    assert needle in path.read_text(), path.read_text()
    # a second sync has nothing to do
    plan = json.loads(invoke("sync", "--plan", "--json").output)
    assert all(s["status"] == "satisfied" for s in plan["steps"] if s["id"].startswith("decider:")), plan["steps"]
    # dropping the deciders removes what hlock wrote (plugins are moved aside)
    Path("harness.toml").unlink()
    assert invoke("pin", "--harness", harness).exit_code == 0
    assert invoke("sync", "--yes").exit_code == 0
    assert not path.is_file() or "decide hook" not in path.read_text()


NODE_HARNESS = r"""
import { spawnSync } from "node:child_process";
const [plugin, kind] = process.argv.slice(2);
const mod = await import(plugin);

// Bun's shell, as far as the plugins use it: each interpolated value is one argument
function shell(strings, ...values) {
  const argv = [];
  strings.forEach((s, i) => {
    argv.push(...s.split(/\s+/).filter(Boolean));
    if (i < values.length) argv.push(String(values[i]));
  });
  const r = spawnSync(argv[0], argv.slice(1), { encoding: "utf8" });
  return { stdout: r.stdout, exitCode: r.status };
}

const handlers = {};
const results = {};
if (kind === "amp") {
  mod.default({ $: async (...a) => shell(...a), on: (name, fn) => (handlers[name] = fn) });
  results.tool = await handlers["tool.call"]({ tool: "Bash", input: { command: "rm -rf /" } }, {});
  results.stop = await handlers["agent.end"]({ status: "done", message: "Fix it", messages: [{ role: "assistant", content: "Looked." }] });
} else if (kind === "opencode") {
  const $ = (...a) => { const r = shell(...a); const p = { quiet: () => p, nothrow: () => p, text: async () => r.stdout }; return p; };
  const hooks = await mod.HlockDeciders({ $ });
  try { await hooks["tool.execute.before"]({ tool: "bash" }, { args: { command: "rm -rf /" } }); results.tool = "ran"; }
  catch (e) { results.tool = { threw: e.message }; }
} else {
  mod.default({ on: (name, fn) => (handlers[name] = fn) });
  results.tool = await handlers["tool_call"]({ toolName: "bash", input: { command: "rm -rf /" } });
}
console.log(JSON.stringify(results));
"""


@pytest.mark.skipif(NODE is None, reason="needs node")
@pytest.mark.parametrize("harness, kind", [("amp", "amp"), ("opencode", "opencode"), ("pi", "pi")])
def test_plugins_ask_hlock_and_act_on_its_answer(project, deny, tmp_path, harness, kind):
    uses = ("approve", "stop") if kind == "amp" else ("approve",)
    write_manifest(deny.url, uses)
    assert invoke("pin", "--harness", harness).exit_code == 0
    plan = json.loads(invoke("sync", "--plan", "--json").output)
    plugin = wired_file(plan)
    assert invoke("sync", "--yes").exit_code == 0
    runner = tmp_path / "run.mjs"
    runner.write_text(NODE_HARNESS)
    r = subprocess.run([NODE, str(runner), plugin.resolve().as_uri(), kind], check=False, capture_output=True, text=True, timeout=60)
    assert r.returncode == 0, r.stderr
    results = json.loads(r.stdout.strip().splitlines()[-1])
    if kind == "amp":
        assert results["tool"]["action"] == "reject-and-continue" and "deny at P=0.97" in results["tool"]["message"]
        # P(done)=0.1: one more turn, with the reason
        assert results["stop"]["action"] == "continue" and results["stop"]["maxContinuations"] == 1
    elif kind == "opencode":
        assert "deny at P=0.97" in results["tool"]["threw"]
    else:
        assert results["tool"]["block"] is True and "deny at P=0.97" in results["tool"]["reason"]
    # the plugin sent the call to the decider
    states = [b["state"] for p, b in deny.requests if "approve" in b["questions"]]
    assert states and states[-1]["input"] == {"command": "rm -rf /"}


@pytest.mark.skipif(NODE is None, reason="needs node")
def test_a_plugin_defers_when_hlock_isnt_there(project, deny, tmp_path, monkeypatch):
    write_manifest(deny.url)
    monkeypatch.setenv("HLOCK_BIN", str(tmp_path / "no-such-hlock"))
    assert invoke("pin", "--harness", "pi").exit_code == 0
    plan = json.loads(invoke("sync", "--plan", "--json").output)
    plugin = wired_file(plan)
    assert invoke("sync", "--yes").exit_code == 0
    runner = tmp_path / "run.mjs"
    runner.write_text(NODE_HARNESS)
    r = subprocess.run([NODE, str(runner), plugin.resolve().as_uri(), "pi"], check=False, capture_output=True, text=True, timeout=60)
    assert r.returncode == 0, r.stderr
    assert json.loads(r.stdout.strip().splitlines()[-1]) == {}, "the call goes ahead as if there were no plugin"


def test_code_harnesses_ask_their_deciders(project, tmp_path):
    """`RunContext.decide` in the entry shim reaches the locked decider."""
    first = Fake(lambda path, body: systemone(body, choice=lambda qid, q, b: next(iter(q["criteria"])), p=0.97))
    deny = first

    shim = Path(__file__).resolve().parent.parent / "crates/harness-lock-core/src/entry_shim.py"
    lock = tmp_path / "deciders.lock"
    lock.write_text(
        'lock-version = 1\ngenerated-by = "test"\ngenerated-at = "2026-10-19T00:00:00Z"\n\n'
        '[[decider]]\nuse = "route"\nprovider = "system-one"\nmodel = "kev-4b"\nprotocol = "systemone"\n'
        f'endpoint = "{deny.url}"\nvia = "env"\n')
    agent = tmp_path / "agent_mod.py"
    agent.write_text(
        "def run(prompt, ctx):\n"
        "    a = ctx.decide('route', options=['planner', 'executor'], state={'step': prompt})\n"
        "    none = ctx.decide('stop', request='x', last_message='y')\n"
        "    return {'output': repr((a.get('answer'), none))}\n")
    env = {**os.environ, "HLOCK_DECIDE_LOCK": str(lock), "HLOCK_BIN": HLOCK, "HLOCK_PROMPT": "write the code",
           "PYTHONPATH": str(tmp_path)}
    r = subprocess.run([sys.executable, "-c", shim.read_text(), "agent_mod:run"], check=False, capture_output=True, text=True, env=env,
                       timeout=60)
    first.close()
    assert r.returncode == 0, r.stderr
    # route: the fake picks the first option it's sent (options go out in name order),
    # at P=0.97; there's no stop decider in the lock: {}
    assert r.stdout.strip() == "('executor', {})"
