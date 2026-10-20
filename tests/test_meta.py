"""`hlock meta`: onboarding, the meta lock, and jobs run by a fake meta harness.

The fake `pi` acts as the agent: it runs the shell snippet in `FAKE_AGENT`, so each
test decides what "the agent did" and checks what hlock makes of it.
"""

import json
import os
import stat
import sys
from pathlib import Path

import pytest
from click.testing import CliRunner

from hlock import Lockfile
from hlock.cli import main


def invoke(*args, env=None):
    return CliRunner().invoke(main, list(args), env=env)


def executable(path: Path, body: str) -> None:
    path.write_text(body)
    path.chmod(path.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)


FAKE_PI = """#!/bin/sh
if [ "$1" = "--version" ]; then echo "1.0.0"; exit 0; fi
echo "AGENT ARGS=$1 PI_DIR=$PI_CODING_AGENT_DIR"
if [ "$1" = "-p" ]; then shift; fi
printf '%s' "$1" > "$PWD/.agent-prompt"
sh -c "$FAKE_AGENT"
"""

PACK = """[pack]
name = "acme/demo"
version = "0.1.0"
domain = "data"

[[task]]
id = "one"
prompt = "one/prompt.md"
expected = { artifacts = ["out.md"] }
"""


@pytest.fixture
def project(tmp_path, monkeypatch):
    for key in list(os.environ):
        if key.startswith(("CLAUDE", "CODEX_", "PI_", "OPENCODE", "HERMES_", "CURSOR_", "HLOCK_", "XDG_", "OLLAMA_", "GOOSE_")) \
                or key.endswith(("_API_KEY", "_TOKEN")):
            monkeypatch.delenv(key)
    home, proj, bindir = tmp_path / "home", tmp_path / "project", tmp_path / "bin"
    for d in (home, proj, bindir):
        d.mkdir()
    executable(bindir / "pi", FAKE_PI)
    executable(bindir / "claude", '#!/bin/sh\nif [ "$1" = "--version" ]; then echo "2.1.0 (Claude Code)"; fi\n')
    monkeypatch.setenv("HOME", str(home))
    monkeypatch.setenv("PATH", f"{bindir}{os.pathsep}/usr/bin{os.pathsep}/bin")
    monkeypatch.setenv("NO_COLOR", "1")
    monkeypatch.setenv("HL_PYTHON", sys.executable)
    monkeypatch.setenv("FAKE_AGENT", "true")
    monkeypatch.chdir(proj)
    return {"home": home, "project": proj, "bin": bindir}


META_ROOT = ".hlock/env/harness.meta/pi/config"


def test_init_with_an_api_key(project, monkeypatch):
    monkeypatch.setenv("OPENAI_API_KEY", "sk-openai-0123456789abcdefABCDEF")
    r = invoke("meta", "init", "--harness", "pi")
    assert r.exit_code == 0, r.output
    assert "OPENAI_API_KEY is set" in r.output and "pinned pi 1.0.0" in r.output
    toml = (project["project"] / "harness.meta.toml").read_text()
    assert 'harness = "pi"' in toml and 'provider = "openai"' in toml and 'name = "gpt-5.5"' in toml
    assert 'key-env = "OPENAI_API_KEY"' in toml
    # the meta harness has its own config; the user's isn't touched
    settings = json.loads((project["project"] / META_ROOT / "settings.json").read_text())
    assert settings == {"defaultProvider": "openai", "defaultModel": "gpt-5.5"}
    assert not (project["home"] / ".pi").exists()
    assert (project["project"] / META_ROOT).parent.joinpath(".gitignore").read_text().endswith("*\n")
    lock = json.loads(Lockfile.load("harness.meta.lock").to_json())
    assert lock["kind"] == "pin" and lock["harness"]["name"] == "pi"
    assert [(x["provider"], x["model"]) for x in lock["llm"]] == [("openai", "gpt-5.5")]
    # the key itself is never written anywhere
    for p in project["project"].rglob("*"):
        if p.is_file():
            assert "sk-openai" not in p.read_text(errors="ignore"), p
    assert invoke("meta", "check").exit_code == 0


def test_init_without_keys_or_runtimes_guides_you(project):
    r = invoke("meta", "init", "--harness", "pi")
    assert r.exit_code == 0, r.output
    assert "anthropic:claude-sonnet-5-5" in r.output and "ANTHROPIC_API_KEY isn't set" in r.output
    assert "export ANTHROPIC_API_KEY=" in r.output
    r = invoke("meta", "run", "bootstrap-evals", "--headless")
    assert r.exit_code == 1 and "ANTHROPIC_API_KEY isn't set" in r.output
    r = invoke("meta", "run", "bootstrap-evals", "--dry-run")  # still shows the plan
    assert r.exit_code == 0 and "ANTHROPIC_API_KEY isn't set" in r.output and "## Job: bootstrap-evals" in r.output


def test_init_refuses_a_proprietary_harness(project):
    r = invoke("meta", "init", "--harness", "claude")
    assert r.exit_code == 1 and "isn't open source" in r.output
    assert not (project["project"] / "harness.meta.toml").exists()


def test_init_with_a_local_model(project):
    executable(project["bin"] / "ollama", "#!/bin/sh\necho 'ollama version is 0.12.0'\n")
    r = invoke("meta", "init", "--harness", "pi", "--local", "--memory-gb", "16")
    assert r.exit_code == 0, r.output
    assert "(local)" in r.output and "ollama pull" in r.output
    toml = (project["project"] / "harness.meta.toml").read_text()
    assert 'runtime = "ollama"' in toml and "key-env" not in toml
    models = json.loads((project["project"] / META_ROOT / "models.json").read_text())
    assert models["providers"]["ollama"]["baseUrl"] == "http://localhost:11434/v1"
    lock = json.loads(Lockfile.load("harness.meta.lock").to_json())
    assert not lock.get("secret")  # a placeholder API key isn't a secret
    # starting over with an API model leaves nothing of the local setup behind
    r = invoke("meta", "init", "--harness", "pi", "--provider", "openai", "--force")
    assert r.exit_code == 0, r.output
    assert not (project["project"] / META_ROOT / "models.json").exists()
    lock = json.loads(Lockfile.load("harness.meta.lock").to_json())
    assert not lock.get("runtime") and [c["id"] for c in lock["config"]] == ["settings"]


def test_check_catches_drift(project, monkeypatch):
    monkeypatch.setenv("ANTHROPIC_API_KEY", "k")
    assert invoke("meta", "init", "--harness", "pi").exit_code == 0
    mm = project["project"] / "harness.meta.toml"
    mm.write_text(mm.read_text().replace("claude-sonnet-5-5", "claude-opus-5-5"))
    r = invoke("meta", "check")
    assert r.exit_code == 1 and "~ model" in r.output and "claude-opus-5-5" in r.output
    assert invoke("meta", "pin").exit_code == 0
    assert invoke("meta", "check").exit_code == 0


def test_bootstrap_pack_headless_verified(project, monkeypatch):
    monkeypatch.setenv("ANTHROPIC_API_KEY", "k")
    assert invoke("meta", "init", "--harness", "pi").exit_code == 0
    # hlock scaffolds the template; the agent fills it in, and asks hlock (which must see
    # the real config root) where pi's config is
    monkeypatch.setenv("FAKE_AGENT", (
        "grep -q 'TODO(hlock)' evals/pack.toml && "
        "mkdir -p evals/one && echo 'do it' > evals/one/prompt.md && "
        f"cat > evals/pack.toml <<'EOF'\n{PACK}EOF\n"
        '"$HL_PYTHON" -m hlock.cli harness show pi --json > .agent-view.json'
    ))
    r = invoke("meta", "run", "bootstrap-evals", "--headless", "--count", "2")
    assert r.exit_code == 0, r.output
    assert "coding template, 2 task slots" in r.output
    assert "✓ verified  1 task in evals" in r.output and "asked for 2, got 1" in r.output
    assert f"AGENT ARGS=-p PI_DIR={project['project'] / META_ROOT}" in r.output
    prompt = (project["project"] / ".agent-prompt").read_text()
    assert "## Job: bootstrap-evals" in prompt and "hlock meta verify bootstrap-evals --out evals" in prompt
    assert "Fill in the eval pack template in `evals`" in prompt and "guide.md" in prompt
    assert "env -u PI_CODING_AGENT_DIR" in prompt
    # hlock run by the agent put the isolation back: it sees the user's real Pi config
    view = json.loads((project["project"] / ".agent-view.json").read_text())
    assert view["paths"]["config_root"] == str(project["home"] / ".pi/agent")
    assert (project["project"] / ".hlock/meta/.gitignore").read_text().endswith("*\n")
    runs = list((project["project"] / ".hlock/meta/runs").iterdir())
    assert len(runs) == 1
    result = json.loads((runs[0] / "result.json").read_text())
    assert result["verdict"]["status"] == "verified" and result["exit_code"] == 0
    assert (runs[0] / "prompt.md").read_text() == prompt and "AGENT ARGS" in (runs[0] / "output.log").read_text()


def test_bootstrap_pack_failure_is_reported(project, monkeypatch):
    monkeypatch.setenv("ANTHROPIC_API_KEY", "k")
    assert invoke("meta", "init", "--harness", "pi").exit_code == 0
    monkeypatch.setenv("FAKE_AGENT", "printf '[pack]\\nname = \"x\"\\n' > evals/pack.toml")
    r = invoke("meta", "run", "bootstrap-evals", "--headless")
    assert r.exit_code == 1, r.output
    assert "✗ failed" in r.output and "[pack] version is missing" in r.output and "the pack has no [[task]] entries" in r.output
    r = invoke("meta", "verify", "bootstrap-evals", "--json")
    assert r.exit_code == 1 and json.loads(r.output)["status"] == "failed"


def test_bootstrap_evals_adds_to_a_complete_pack(project, monkeypatch):
    monkeypatch.setenv("ANTHROPIC_API_KEY", "k")
    assert invoke("meta", "init", "--harness", "pi").exit_code == 0
    (project["project"] / "evals/one").mkdir(parents=True)
    (project["project"] / "evals/one/prompt.md").write_text("do it\n")
    (project["project"] / "evals/pack.toml").write_text(PACK)
    prep = json.loads(invoke("meta", "run", "bootstrap-evals", "--dry-run", "--json").output)
    assert "Add tasks to the eval pack in `evals` (it has one)" in prep["prompt"]
    assert (project["project"] / "evals/pack.toml").read_text() == PACK  # nothing scaffolded over it


def test_bootstrap_evals_verify_uses_the_manifest_judge(project, monkeypatch):
    monkeypatch.setenv("ANTHROPIC_API_KEY", "k")
    assert invoke("meta", "init", "--harness", "pi").exit_code == 0
    pack = project["project"] / "evals"
    (pack / "one").mkdir(parents=True)
    (pack / "one/prompt.md").write_text("do it\n")
    (pack / "one/rubric.md").write_text("- Names the cause.\n")
    (pack / "pack.toml").write_text(PACK + 'graders = ["judge:one/rubric.md"]\n')
    r = invoke("meta", "verify", "bootstrap-evals", "--json")
    assert "judge graders need a model" in r.output
    (project["project"] / "harness.toml").write_text('[evals]\npacks = ["./evals"]\n\n[evals.judge]\nmodel = "openai:gpt-x"\n')
    r = invoke("meta", "verify", "bootstrap-evals", "--json")
    assert r.exit_code == 0 and "judge graders need a model" not in r.output, r.output


def test_bootstrap_evals_verify_asks_a_decider_about_each_task(project, monkeypatch):
    from fake_models import Fake, systemone

    # the prompt is fine; the graders and the solution aren't
    fake = Fake(lambda path, body: systemone(body, noul=lambda qid, q, b: 0.9 if qid == "alone" else 0.2))
    try:
        monkeypatch.setenv("ANTHROPIC_API_KEY", "k")
        assert invoke("meta", "init", "--harness", "pi").exit_code == 0
        meta = project["project"] / "harness.meta.toml"
        meta.write_text(meta.read_text() + f'\n[deciders.default]\nmodel = "system-one:kev-4b"\nbase-url = "{fake.url}"\n')
        pack = project["project"] / "evals"
        (pack / "one/solution").mkdir(parents=True)
        (pack / "one/prompt.md").write_text("Write out.md with the answer.\n")
        (pack / "one/solution/out.md").write_text("42\n")
        (pack / "pack.toml").write_text(PACK + 'solution = "one/solution/"\n')
        r = invoke("meta", "verify", "bootstrap-evals", "--json")
        assert r.exit_code == 0, r.output
        details = json.loads(r.output)["details"]
        assert "warning: one: its graders may not check what the prompt asks for (kev-4b-q8 P=0.20)" in details
        assert "warning: one: its solution may not do what the prompt asks (kev-4b-q8 P=0.20)" in details
        assert not any("lean on context" in d for d in details)
        assert fake.requests[0][1]["state"]["solution"] == {"out.md": "42\n"}
    finally:
        fake.close()
    meta.write_text(meta.read_text() + '\n[deciders.approve]\nmodel = "ollama:kev:4b"\n')
    assert "the meta harness takes [deciders.default]" in invoke("meta", "verify", "bootstrap-evals").output


def test_install_job(project, monkeypatch):
    home = project["home"]
    (home / ".claude/skills/my-skill").mkdir(parents=True)
    (home / ".claude/skills/my-skill/SKILL.md").write_text("---\nname: my-skill\ndescription: d\n---\nbody\n")
    (home / ".claude/settings.json").write_text(json.dumps({"model": "opus"}))
    assert invoke("pin", "--harness", "claude").exit_code == 0
    monkeypatch.setenv("ANTHROPIC_API_KEY", "k")
    assert invoke("meta", "init", "--harness", "pi").exit_code == 0
    # the setup is restored into an isolated root, which doesn't have the skill yet
    r = invoke("meta", "verify", "install")
    assert r.exit_code == 1 and "my-skill" in r.output
    prompt = json.loads(invoke("meta", "run", "install", "--dry-run", "--headless", "--json").output)["prompt"]
    assert "my-skill" in prompt and "hlock sync --yes" in prompt and "isolated root under `.hlock/env`" in prompt
    monkeypatch.setenv("FAKE_AGENT", '"$HL_PYTHON" -m hlock.cli sync --yes')
    r = invoke("meta", "run", "install", "--headless")
    assert r.exit_code == 0, r.output
    assert (project["project"] / ".hlock/env/claude-code/config/skills/my-skill/SKILL.md").is_file()
    assert invoke("meta", "verify", "install").exit_code == 0


def test_update_job_reports_and_repins(project, monkeypatch):
    home = project["home"]
    (home / ".claude").mkdir()
    (home / ".claude/settings.json").write_text(json.dumps({"model": "opus"}))
    assert invoke("pin", "--harness", "claude").exit_code == 0
    monkeypatch.setenv("ANTHROPIC_API_KEY", "k")
    assert invoke("meta", "init", "--harness", "pi").exit_code == 0
    r = invoke("meta", "run", "update", "--dry-run", "--headless", "--json")
    assert "may be nothing to update" in json.loads(r.output)["prompt"]
    monkeypatch.setenv("FAKE_AGENT", (
        "mkdir -p \"$HOME/.claude/skills/new-skill\" && "
        "printf -- '---\\nname: new-skill\\ndescription: d\\n---\\n' > \"$HOME/.claude/skills/new-skill/SKILL.md\""
    ))
    r = invoke("meta", "run", "update", "--headless")
    assert r.exit_code == 0, r.output
    assert "1 change since harness.lock" in r.output and "+ skill new-skill" in r.output and "next → hlock pin" in r.output
    assert invoke("check").exit_code == 1  # not re-pinned
    monkeypatch.setenv("FAKE_AGENT", "true")
    r = invoke("meta", "run", "update", "--headless", "--pin")
    assert r.exit_code == 0 and "re-pinned harness.lock" in r.output
    assert invoke("check").exit_code == 0


def test_install_and_update_verify_ask_a_decider(project, monkeypatch):
    from fake_models import Fake, systemone

    home = project["home"]
    (home / ".claude/skills/my-skill").mkdir(parents=True)
    (home / ".claude/skills/my-skill/SKILL.md").write_text("---\nname: my-skill\ndescription: d\n---\nbody\n")
    (home / ".claude/settings.json").write_text(json.dumps({"model": "opus"}))
    assert invoke("pin", "--harness", "claude").exit_code == 0
    monkeypatch.setenv("ANTHROPIC_API_KEY", "k")
    assert invoke("meta", "init", "--harness", "pi").exit_code == 0
    fake = Fake(lambda path, body: systemone(body, noul=0.8, choice="the network", p=0.7))
    try:
        meta = project["project"] / "harness.meta.toml"
        meta.write_text(meta.read_text() + f'\n[deciders.default]\nmodel = "system-one:kev-4b"\nbase-url = "{fake.url}"\n')
        # install: the skill isn't in the isolated root yet; whose move is it?
        details = json.loads(invoke("meta", "verify", "install", "--json").output)["details"]
        assert any("my-skill" in d and d.endswith("— needs the network (P=0.70)") for d in details), details
        # update: a change since the pin, read as risky
        (home / ".claude/skills/new-skill").mkdir(parents=True)
        (home / ".claude/skills/new-skill/SKILL.md").write_text("---\nname: new-skill\ndescription: d\n---\n")
        details = json.loads(invoke("meta", "verify", "update", "--json").output)["details"]
        assert "warning: may break something: + skill new-skill (user) (P=0.80)" in details, details
    finally:
        fake.close()


def test_update_needs_a_pinned_lock(project, monkeypatch):
    (project["project"] / "harness.lock").write_text(
        "lock-version = 1\ngenerated-by = \"x\"\ngenerated-at = \"\"\n[harness]\nname = \"reference\"\n[evidence]\ntrials = 2\n")
    monkeypatch.setenv("ANTHROPIC_API_KEY", "k")
    assert invoke("meta", "init", "--harness", "pi").exit_code == 0
    r = invoke("meta", "run", "update", "--headless")
    assert r.exit_code == 1 and "resolved lock" in r.output


def test_custom_job_and_jobs_list(project, monkeypatch):
    monkeypatch.setenv("ANTHROPIC_API_KEY", "k")
    assert invoke("meta", "init", "--harness", "pi").exit_code == 0
    mm = project["project"] / "harness.meta.toml"
    mm.write_text(mm.read_text() + '\n[jobs.notes]\ndescription = "Write notes"\nprompt = "Write NOTES.md about {lock}"\n'
                  'verify = "test -s NOTES.md"\n')
    r = invoke("meta", "jobs", "--json")
    assert [j["name"] for j in json.loads(r.output)] == ["install", "bootstrap-evals", "update", "notes"]
    monkeypatch.setenv("FAKE_AGENT", "echo hi > NOTES.md")
    r = invoke("meta", "run", "notes", "--headless", "be", "brief")
    assert r.exit_code == 0, r.output
    prompt = (project["project"] / ".agent-prompt").read_text()
    assert "Write NOTES.md about harness.lock" in prompt and "be brief" in prompt
    r = invoke("meta", "run", "nope", "--headless")
    assert r.exit_code == 1 and "unknown meta job" in r.output


def test_interactive_and_launch_use_the_meta_root(project, monkeypatch):
    monkeypatch.setenv("ANTHROPIC_API_KEY", "k")
    assert invoke("meta", "init", "--harness", "pi").exit_code == 0
    r = invoke("meta", "run", "bootstrap-evals", "--dry-run", "--json")
    prep = json.loads(r.output)
    assert prep["mode"] == "interactive" and prep["argv"][1:] == [prep["prompt"]]  # first message, no -p
    assert prep["env"]["PI_CODING_AGENT_DIR"] == str(project["project"] / META_ROOT)
    assert json.loads(prep["env"]["HLOCK_META_RESTORE"]) == {"PI_CODING_AGENT_DIR": None}
    r = invoke("meta", "launch", "--dry-run", "--json")
    assert r.exit_code == 0, r.output
    assert json.loads(r.output)["env"] == {"PI_CODING_AGENT_DIR": str(project["project"] / META_ROOT)}
    assert invoke("meta", "launch", "--in-place").exit_code == 1


def test_meta_and_project_locks_vendor_apart(project, monkeypatch):
    # the project pins the user's own Pi; the meta lock pins the meta harness's Pi
    pi_home = project["home"] / ".pi/agent"
    pi_home.mkdir(parents=True)
    (pi_home / "settings.json").write_text(json.dumps({"defaultProvider": "openai", "defaultModel": "gpt-5"}))
    assert invoke("pin", "--harness", "pi").exit_code == 0
    monkeypatch.setenv("ANTHROPIC_API_KEY", "k")
    assert invoke("meta", "init", "--harness", "pi").exit_code == 0
    r = invoke("meta", "pin")
    assert "vendored 1 files into .hlock/vendor/meta/pi/" in r.output, r.output
    vendor = project["project"] / ".hlock/vendor"
    assert json.loads((vendor / "pi/config/settings.json").read_text())["defaultModel"] == "gpt-5"
    assert json.loads((vendor / "meta/pi/config/settings.json").read_text())["defaultModel"] == "claude-sonnet-5-5"
    assert invoke("check").exit_code == 0 and invoke("meta", "check").exit_code == 0


def test_no_meta_lock(project):
    r = invoke("meta", "run", "install")
    assert r.exit_code == 1 and "hlock meta init" in r.output
