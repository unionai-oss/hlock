"""Autonomous agents (OpenClaw, nanobot, PicoClaw, ZeroClaw, IronClaw): `harness list`,
`pin`, `launch`/`run`, and `localize`, against a fake home with fake binaries."""

import json
import os
import stat
from pathlib import Path

import pytest
from click.testing import CliRunner

from hlock import Lockfile, launch_prepare
from hlock.cli import main

GH_TOKEN = "ghp_0123456789abcdefABCDEF0123456789"

OPENCLAW_CONFIG = """{
  // written by `openclaw onboard`, then edited by hand
  agents: {
    defaults: {
      model: { primary: 'anthropic/claude-opus-4-6', fallbacks: ['openai/gpt-5.4'] },
      workspace: '~/.openclaw/workspace',
    },
  },
  models: {
    providers: {
      vllm: { baseUrl: 'http://gpu-box:8000/v1', api: 'openai-completions', models: [{ id: 'qwen3-coder', name: 'Qwen' }] },
    },
  },
  mcp: {
    servers: {
      github: { command: 'npx', args: ['-y', '@modelcontextprotocol/server-github'], env: { GITHUB_TOKEN: 'GH_TOKEN' } },
      docs: { transport: 'streamable-http', url: 'https://docs.example.com/mcp' },
    },
  },
}""".replace("GH_TOKEN", GH_TOKEN)


def invoke(*args):
    return CliRunner().invoke(main, list(args))


def executable(path: Path, body: str) -> None:
    path.write_text(body)
    path.chmod(path.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)


def load(path: str) -> dict:
    return json.loads(Lockfile.load(path).to_json())


def all_text(root: Path) -> str:
    return "\n".join(p.read_text(errors="ignore") for p in root.rglob("*") if p.is_file())


@pytest.fixture
def home(tmp_path, monkeypatch):
    for key in list(os.environ):
        if key.startswith(("CLAUDE", "CODEX_", "PI_", "OPENCODE", "HERMES_", "CURSOR_", "HLOCK_", "XDG_", "OLLAMA_",
                           "OPENCLAW_", "PICOCLAW_", "ZEROCLAW_", "IRONCLAW_")):
            monkeypatch.delenv(key)
    home, project, bindir = tmp_path / "home", tmp_path / "project", tmp_path / "bin"
    for d in (home, project, bindir):
        d.mkdir()
    monkeypatch.setenv("HOME", str(home))
    monkeypatch.setenv("PATH", f"{bindir}{os.pathsep}/usr/bin{os.pathsep}/bin")
    monkeypatch.setenv("NO_COLOR", "1")
    monkeypatch.chdir(project)

    # OpenClaw: a JSON5 config, a workspace with persona files and agent-written memory
    executable(bindir / "openclaw", "#!/bin/sh\n"
               'if [ "$1" = "--version" ]; then echo "OpenClaw 2026.10.14"; exit 0; fi\n'
               'echo "STATE=$OPENCLAW_STATE_DIR ARGS=$*"\n')
    oc = home / ".openclaw"
    (oc / "workspace/skills/inbox-zero").mkdir(parents=True)
    (oc / "workspace/skills/inbox-zero/SKILL.md").write_text("---\nname: inbox-zero\ndescription: d\n---\nbody\n")
    (oc / "openclaw.json").write_text(OPENCLAW_CONFIG)
    (oc / "workspace/AGENTS.md").write_text("check the inbox every morning\n")
    (oc / "workspace/SOUL.md").write_text("calm and brief\n")
    (oc / "workspace/MEMORY.md").write_text("the user likes tea\n")
    (oc / "workspace/memory").mkdir()
    (oc / "workspace/memory/2026-10-18.md").write_text("did things\n")
    return home


def test_harness_list_groups_autonomous_agents(home):
    rows = {r["id"]: r for r in json.loads(invoke("harness", "list", "--json").output)}
    for hid in ("openclaw", "nanobot", "picoclaw", "zeroclaw", "ironclaw", "hermes"):
        assert rows[hid]["kind"] == "cli" and rows[hid]["autonomous"] is True, hid
    assert rows["claude-code"]["autonomous"] is False
    assert rows["openclaw"]["probe"]["version"] == "2026.10.14"

    r = invoke("harness", "list")
    assert r.exit_code == 0, r.output
    clis, autonomous = r.output.split("Autonomous agents")
    assert "claude-code" in clis and "openclaw" not in clis
    assert "openclaw" in autonomous.split("Frameworks")[0]

    r = invoke("harness", "show", "clawdbot")
    assert r.exit_code == 0, r.output
    assert "OpenClaw" in r.output and "autonomous agent" in r.output and "OPENCLAW_STATE_DIR" in r.output


def test_pin_openclaw(home):
    r = invoke("pin", "--user", "--harness", "openclaw")
    assert r.exit_code == 0, r.output
    assert "pinned openclaw 2026.10.14" in r.output
    lock = load("harness.lock")
    (llm,) = [x for x in lock["llm"] if x["role"] == "default"]
    assert (llm["provider"], llm["model"]) == ("anthropic", "claude-opus-4-6")
    tools = {t["name"]: t for t in lock["tool"]}
    assert tools["github"]["source"] == "npm:@modelcontextprotocol/server-github"
    assert tools["github"]["env"]["GITHUB_TOKEN"].startswith("${HLOCK_SECRET_")
    assert tools["docs"]["transport"] == "http" and tools["docs"]["url"] == "https://docs.example.com/mcp"
    assert [s["name"] for s in lock["skill"]] == ["inbox-zero"]
    # persona files are pinned; what the agent writes as it runs is not
    names = {i["name"] for i in lock["instructions"]}
    assert names == {"{root}/workspace/AGENTS.md", "{root}/workspace/SOUL.md"}
    (config,) = lock["config"]
    assert config["format"] == "json5" and config["restore"] == "whole"
    assert GH_TOKEN not in all_text(Path.cwd())

    assert invoke("check").exit_code == 0
    # the agent remembering things isn't drift
    (home / ".openclaw/workspace/MEMORY.md").write_text("the user likes coffee now\n")
    assert invoke("check").exit_code == 0
    # a changed persona is
    (home / ".openclaw/workspace/SOUL.md").write_text("loud\n")
    r = invoke("check")
    assert r.exit_code == 1 and "SOUL.md" in r.output


def test_launch_and_run_openclaw(home):
    assert invoke("pin", "--user", "--harness", "openclaw").exit_code == 0
    r = invoke("launch", "--dry-run", "--json")
    assert r.exit_code == 0, r.output
    prep = json.loads(r.output)
    root = Path(prep["root"])
    assert prep["env"] == {"OPENCLAW_STATE_DIR": str(root)}
    config = json.loads((root / "openclaw.json").read_text())  # JSON is valid JSON5
    assert config["agents"]["defaults"]["model"]["primary"] == "anthropic/claude-opus-4-6"
    # OpenClaw expands ${VAR} in its config, so the secret travels in the environment
    assert config["mcp"]["servers"]["github"]["env"]["GITHUB_TOKEN"] == "${HLOCK_SECRET_GITHUB_GITHUB_TOKEN}"
    assert json.loads(launch_prepare())["secret_env"]["HLOCK_SECRET_GITHUB_GITHUB_TOKEN"] == GH_TOKEN
    assert (root / "workspace/SOUL.md").read_text() == "calm and brief\n"

    r = invoke("run", "--json", "triage my inbox")
    assert r.exit_code == 0, r.output
    out = json.loads(r.output)
    assert out["harness"] == "openclaw" and "ARGS=agent exec triage my inbox" in out["output"]


def test_launch_translates_into_openclaw(home):
    executable(Path(os.environ["PATH"].split(os.pathsep)[0]) / "claude", '#!/bin/sh\necho "2.1.0 (Claude Code)"\n')
    (home / ".claude").mkdir()
    (home / ".claude/settings.json").write_text(json.dumps({"model": "opus"}))
    (home / ".claude/CLAUDE.md").write_text("be terse\n")
    (home / ".claude.json").write_text(json.dumps({"mcpServers": {
        "remote": {"type": "sse", "url": "https://x.example.com/sse"}}}))
    assert invoke("pin", "--user", "--harness", "claude").exit_code == 0
    r = invoke("launch", "openclaw", "--dry-run", "--json")
    assert r.exit_code == 0, r.output
    prep = json.loads(r.output)
    root = Path(prep["root"])
    config = json.loads((root / "openclaw.json").read_text())
    assert config["mcp"]["servers"]["remote"] == {"transport": "sse", "url": "https://x.example.com/sse"}
    assert (root / "workspace/AGENTS.md").read_text() == "be terse\n"
    # `--model` is a subcommand flag in OpenClaw, so it's not passed, and launch says so
    assert "--model" not in prep["argv"]
    assert any("not the locked anthropic/opus" in n for n in prep["notes"])


def test_pin_nanobot_picoclaw_zeroclaw_ironclaw(home):
    bindir = Path(os.environ["PATH"].split(os.pathsep)[0])
    executable(bindir / "nanobot", "#!/bin/sh\necho 'nanobot v0.3.5'\n")
    executable(bindir / "picoclaw", "#!/bin/sh\n"
               'if [ "$1" = "version" ]; then echo "picoclaw 0.4.2"; exit 0; fi\necho "ARGS=$*"\n')
    executable(bindir / "zeroclaw", "#!/bin/sh\necho 'zeroclaw 0.8.6'\n")
    executable(bindir / "ironclaw", "#!/bin/sh\necho 'ironclaw 1.4.1'\n")

    nb = home / ".nanobot"
    (nb / "workspace").mkdir(parents=True)
    (nb / "config.json").write_text(json.dumps({
        "agents": {"defaults": {"model": "anthropic/claude-opus-4-5", "provider": "anthropic"}},
        "providers": {"vllm": {"apiBase": "http://127.0.0.1:8000/v1", "apiKey": "EMPTY"}},
        "tools": {"mcpServers": {"github": {"command": "npx", "args": ["-y", "@modelcontextprotocol/server-github"]}}},
    }))
    (nb / "workspace/SOUL.md").write_text("helpful\n")
    r = invoke("pin", "--user", "--harness", "nanobot", "-o", "nanobot.lock")
    assert r.exit_code == 0, r.output
    lock = load("nanobot.lock")
    assert lock["llm"][0]["model"] == "anthropic/claude-opus-4-5"
    assert [t["name"] for t in lock["tool"]] == ["github"]
    assert [i["name"] for i in lock["instructions"]] == ["{root}/workspace/SOUL.md"]

    pc = home / ".picoclaw"
    (pc / "workspace").mkdir(parents=True)
    (pc / "config.json").write_text(json.dumps({
        "version": 3,
        "agents": {"defaults": {"model_name": "fast"}},
        "model_list": [{"model_name": "fast", "provider": "openai", "model": "gpt-5.4"}],
        "tools": {"mcp": {"enabled": True, "servers": {"fs": {"command": "npx", "args": ["-y", "server-fs"]}}}},
    }))
    (pc / "workspace/AGENT.md").write_text("---\nname: pico\n---\nbe small\n")
    r = invoke("pin", "--user", "--harness", "picoclaw", "-o", "picoclaw.lock")
    assert r.exit_code == 0, r.output
    lock = load("picoclaw.lock")
    assert lock["harness"]["version"] == "0.4.2" and lock["llm"][0]["model"] == "fast"
    assert [t["name"] for t in lock["tool"]] == ["fs"]
    assert [i["name"] for i in lock["instructions"]] == ["{root}/workspace/AGENT.md"]
    # the subcommand comes before `-m`, so `--model` lands on `picoclaw agent`
    prep = json.loads(invoke("--lock", "picoclaw.lock", "launch", "--dry-run", "--json").output)
    assert prep["argv"][1:] == ["agent"]  # a pinned lock restores the model with its config
    out = json.loads(invoke("--lock", "picoclaw.lock", "run", "--json", "hi").output)
    assert "ARGS=agent -m hi" in out["output"]

    # ZeroClaw lists MCP servers as [[mcp.servers]] tables with a name
    zc = home / ".zeroclaw"
    (zc / "agents/assistant/workspace").mkdir(parents=True)
    (zc / "config.toml").write_text(
        'schema_version = 3\n'
        '[providers.models.anthropic.home]\nmodel = "claude-sonnet-4-5"\n'
        '[agents.assistant]\nmodel_provider = "anthropic.home"\nmcp_bundles = ["files"]\n'
        '[[mcp.servers]]\nname = "filesystem"\ncommand = "npx"\nargs = ["-y", "@modelcontextprotocol/server-filesystem"]\n'
        '[[mcp.servers]]\nname = "search"\ntransport = "http"\nurl = "https://search.example.com/mcp"\n')
    (zc / "agents/assistant/workspace/IDENTITY.md").write_text("Zed\n")
    r = invoke("pin", "--user", "--harness", "zeroclaw", "-o", "zeroclaw.lock")
    assert r.exit_code == 0, r.output
    lock = load("zeroclaw.lock")
    tools = {t["name"]: t for t in lock["tool"]}
    assert tools["filesystem"]["command"] == "npx" and tools["search"]["transport"] == "http"
    assert [i["name"] for i in lock["instructions"]] == ["{root}/agents/assistant/workspace/IDENTITY.md"]

    ic = home / ".ironclaw/reborn"
    ic.mkdir(parents=True)
    (ic / "config.toml").write_text('[llm.default]\nprovider_id = "anthropic"\nmodel = "claude-sonnet-4-20250514"\n'
                                    'api_key_env = "ANTHROPIC_API_KEY"\n')
    r = invoke("pin", "--user", "--harness", "ironclaw", "-o", "ironclaw.lock")
    assert r.exit_code == 0, r.output
    (llm,) = load("ironclaw.lock")["llm"]
    assert (llm["provider"], llm["model"]) == ("anthropic", "claude-sonnet-4-20250514")


@pytest.fixture
def local(home):
    bindir = Path(os.environ["PATH"].split(os.pathsep)[0])
    executable(bindir / "ollama", "#!/bin/sh\necho 'ollama version is 0.12.0'\n")
    m = home / ".ollama/models/manifests/registry.ollama.ai/library/qwen3/8b"
    m.parent.mkdir(parents=True)
    m.write_text(json.dumps({"config": {"size": 1}, "layers": [{"size": 5_200_000_000}]}))
    assert invoke("pin", "--user", "--harness", "openclaw").exit_code == 0
    return home


def test_localize_openclaw_uses_ollamas_native_api(local):
    r = invoke("localize", "--harness", "openclaw", "--models", "qwen3:8b")
    assert r.exit_code == 0, r.output
    lock = load("harness.local.lock")
    assert lock["harness"]["name"] == "openclaw"
    # the same harness: the local model settings go into the config file it already locks,
    # not into a second entry for the same file
    (cfg,) = [c for c in lock["config"] if c["id"] == "config"]
    assert "/localize/" in cfg["vendored"] and cfg["restore"] != "pointers"
    doc = json.loads(Path(cfg["vendored"]).read_text())
    # not the /v1 endpoint, which makes tool calling unreliable in OpenClaw
    assert doc["models"]["providers"]["ollama"]["api"] == "ollama"
    assert doc["models"]["providers"]["ollama"]["baseUrl"] == "http://localhost:11434"
    assert doc["agents"]["defaults"]["model"]["primary"] == "ollama/qwen3:8b"


def test_localize_zeroclaw_says_what_to_set(local):
    executable(Path(os.environ["PATH"].split(os.pathsep)[0]) / "zeroclaw", "#!/bin/sh\necho 'zeroclaw 0.8.6'\n")
    r = invoke("localize", "--harness", "zeroclaw", "--models", "qwen3:8b", "-o", "zc.lock")
    assert r.exit_code == 0, r.output
    assert "point it at http://localhost:11434/v1 yourself" in " ".join(r.output.split())
    assert load("zc.lock")["harness"]["name"] == "zeroclaw"


def test_localize_onto_another_agent_keeps_each_instruction_file(local):
    executable(Path(os.environ["PATH"].split(os.pathsep)[0]) / "nanobot", "#!/bin/sh\necho 'nanobot v0.3.5'\n")
    r = invoke("localize", "--harness", "nanobot", "--models", "qwen3:8b", "-o", "nb.lock")
    assert r.exit_code == 0, r.output
    # AGENTS.md and SOUL.md each keep their own file, rather than both becoming AGENTS.md
    names = sorted(i["name"] for i in load("nb.lock")["instructions"])
    assert names == ["{root}/workspace/AGENTS.md", "{root}/workspace/SOUL.md"]


def test_localize_openclaw_on_another_runtime(local):
    r = invoke("localize", "--harness", "openclaw", "--runtime", "lmstudio", "--models", "qwen/qwen3-8b", "-o", "lms.lock")
    assert r.exit_code == 0, r.output
    (cfg,) = [c for c in load("lms.lock")["config"] if c["id"] == "config"]
    doc = json.loads(Path(cfg["vendored"]).read_text())
    assert doc["models"]["providers"]["lmstudio"]["api"] == "openai-completions"
    assert doc["models"]["providers"]["lmstudio"]["baseUrl"].endswith(":1234/v1")
