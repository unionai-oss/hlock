"""The Union artifact-store provider: `hlock push/pull union:…`, against an in-process mock
Union store that speaks the flyteidl2 artifact service (Connect-JSON) with OAuth, the way
`unionai-oss/hlock-cloud`'s `union.ts` does. A flyte config.yaml points hlock at the store;
`FLYTE_API_KEY` (client_credentials) authenticates it."""

import base64
import json
import os
import re
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlparse

import pytest
from click.testing import CliRunner

from hlock import is_union_source
from hlock.cli import main

LOCK = """lock-version = 1
generated-by = "test"
generated-at = ""

[harness]
name = "pi"

[[skill]]
name = "review"
origin = "local"
vendored = ".hlock/vendor/pi/skills/review"
"""


def invoke(*args):
    return CliRunner().invoke(main, list(args))


def plain(output: str) -> str:
    return re.sub(r"\x1b\[[0-9;]*m", "", output)


class UnionStore:
    """An in-memory Union artifact store: `name@version` → bytes, write-once."""

    def __init__(self):
        self.artifacts: dict[str, bytes] = {}
        self.tags: dict[str, str] = {}


def handler_for(store: UnionStore, base: str):
    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *a):
            pass

        def send(self, status, body=b"", ctype="application/json"):
            if isinstance(body, (dict, list)):
                body = json.dumps(body).encode()
            self.send_response(status)
            self.send_header("content-type", ctype)
            self.send_header("content-length", str(len(body)))
            self.send_header("connection", "close")
            self.end_headers()
            self.wfile.write(body)

        def body(self) -> bytes:
            return self.rfile.read(int(self.headers.get("content-length") or 0))

        def do_GET(self):
            path = urlparse(self.path).path
            if path.endswith("/.well-known/oauth-authorization-server"):
                return self.send(200, {"tokenEndpoint": f"{base}/token"})
            self.send(404, {"error": "not found"})

        def do_POST(self):
            path = urlparse(self.path).path
            body = self.body()
            if path.endswith("/flyteidl.service.AuthMetadataService/GetPublicClientConfig"):
                return self.send(200, {"scopes": ["all"], "authorizationMetadataKey": "authorization"})
            if path.endswith("/token"):
                # client_credentials: a form body, not JSON
                return self.send(200, {"access_token": "tok", "expires_in": 3600})
            data = json.loads(body or b"{}")
            if path.endswith("/flyteidl2.artifact.ArtifactService/CreateArtifact"):
                aid = data.get("artifactId", {})
                name = (aid.get("name") or {}).get("name")
                version = aid.get("version")
                k = f"{name}@{version}"
                if k in store.artifacts:
                    return self.send(409, {"detail": "artifact already exists"})
                bin = data.get("spec", {}).get("value", {}).get("scalar", {}).get("binary", {})
                store.artifacts[k] = base64.b64decode(bin.get("value") or "")
                store.tags[k] = bin.get("tag") or ""
                return self.send(200, {})
            if path.endswith("/flyteidl2.artifact.ArtifactService/GetArtifact"):
                aid = data.get("name", {})
                name, version = aid.get("name"), data.get("version")
                k = f"{name}@{version}"
                if k not in store.artifacts:
                    return self.send(404, {"detail": "artifact not found"})
                return self.send(200, {"artifact": {"spec": {"value": {"scalar": {
                    "binary": {"value": base64.b64encode(store.artifacts[k]).decode(), "tag": store.tags.get(k, "")}
                }}}}})
            self.send(404, {"error": "not found"})

    return Handler


@pytest.fixture
def world(tmp_path, monkeypatch):
    for key in list(os.environ):
        if key.startswith(("HLOCK_", "GIT_", "FLYTE_")):
            monkeypatch.delenv(key)
    monkeypatch.setenv("GIT_CEILING_DIRECTORIES", str(tmp_path.parent))
    monkeypatch.setenv("GIT_CONFIG_NOSYSTEM", "1")
    monkeypatch.setenv("HLOCK_HOME", str(tmp_path / "hlock-home"))
    monkeypatch.setenv("HLOCK_KEYCHAIN", "0")
    monkeypatch.setenv("HOME", str(tmp_path / "home"))
    monkeypatch.setenv("NO_COLOR", "1")
    (tmp_path / "home").mkdir()

    store = UnionStore()
    server = ThreadingHTTPServer(("127.0.0.1", 0), handler_for(store, "http://placeholder"))
    port = server.server_address[1]
    base = f"http://127.0.0.1:{port}"
    server.RequestHandlerClass = handler_for(store, base)
    threading.Thread(target=server.serve_forever, daemon=True).start()

    endpoint = f"127.0.0.1:{port}"
    monkeypatch.setenv("FLYTE_API_KEY", base64.b64encode(f"{endpoint}:client:secret:demo".encode()).decode())

    author = tmp_path / "author"
    (author / ".hlock/vendor/pi/skills/review").mkdir(parents=True)
    (author / ".hlock/vendor/pi/skills/review/SKILL.md").write_text("Review the diff.\n")
    (author / "harness.lock").write_text(LOCK)
    (author / "harness.toml").write_text('[project]\nname = "My Setup"\n')
    (author / ".flyte").mkdir()
    (author / ".flyte/config.yaml").write_text(
        f"admin:\n  endpoint: {endpoint}\ntask:\n  org: demo\n  project: hlock-cloud\n  domain: development\n")
    me = tmp_path / "me"
    me.mkdir()
    monkeypatch.chdir(me)
    yield {"store": store, "author": author, "me": me, "tmp": tmp_path, "config": str(author / ".flyte/config.yaml"),
           "endpoint": endpoint}
    server.shutdown()


def test_union_source_is_recognized():
    assert is_union_source("union:alice/review@1")
    assert not is_union_source("github:alice/review")
    assert not is_union_source("alice/review")


def test_push_union_dry_run_needs_no_key_and_shows_the_store(world, monkeypatch):
    monkeypatch.delenv("FLYTE_API_KEY")
    monkeypatch.chdir(world["author"])
    r = invoke("push", "union:alice/review", "--config", world["config"], "--dry-run", "--json")
    assert r.exit_code == 0, r.output
    out = json.loads(r.output)
    assert out["provider"] == "union"
    assert out["package"] == "alice/review"
    assert out["store"]["project"] == "hlock-cloud"
    assert out["store"]["endpoint"] == world["endpoint"]


def test_push_then_pull_union_round_trips(world, monkeypatch):
    author = world["author"]
    monkeypatch.chdir(author)
    r = invoke("push", "union:alice/review", "--config", world["config"], "--json")
    assert r.exit_code == 0, r.output
    out = json.loads(r.output)
    assert out["published"] is True
    version = out["version"]
    name = f"setup.alice.review@{version}"
    assert name in world["store"].artifacts, world["store"].artifacts.keys()

    # pull it back into the store, by name
    monkeypatch.chdir(world["me"])
    p = invoke("pull", f"union:alice/review@{version}", "--config", world["config"], "--as", "review")
    assert p.exit_code == 0, p.output
    pulled = world["tmp"] / "hlock-home" / "pulls" / "review"
    lock = (pulled / "harness.lock").read_text()
    assert 'name = "pi"' in lock
    # the vendored skill came along, under the store
    skill = pulled / ".hlock/vendor/pi/skills/review/SKILL.md"
    assert skill.read_text() == "Review the diff.\n"


def test_pull_union_needs_a_version(world):
    r = invoke("pull", "union:alice/review", "--config", world["config"])
    assert r.exit_code != 0
    assert "needs an explicit @VERSION" in plain(r.output)


def test_pull_union_requires_the_api_key(world, monkeypatch):
    monkeypatch.delenv("FLYTE_API_KEY")
    r = invoke("pull", "union:alice/review@1", "--config", world["config"])
    assert r.exit_code != 0
    assert "FLYTE_API_KEY" in plain(r.output)


def test_no_flyte_config_is_a_clear_error(world):
    r = invoke("push", "union:alice/review", "--config", "/nonexistent/config.yaml", "--dry-run")
    assert r.exit_code != 0
    assert "config" in plain(r.output).lower()


def test_a_version_cannot_be_overwritten(world, monkeypatch):
    monkeypatch.chdir(world["author"])
    r = invoke("push", "union:alice/review", "--version", "1.0.0", "--config", world["config"])
    assert r.exit_code == 0, r.output
    r2 = invoke("push", "union:alice/review", "--version", "1.0.0", "--config", world["config"], "--json")
    assert r2.exit_code == 0, r2.output
    out = json.loads(r2.output)
    assert out["already_published"] is True


def test_a_union_pull_can_be_refreshed(world, monkeypatch):
    # the lock's [pulled] from = "union:…@version"; a bare `hlock pull` refreshes it through union
    monkeypatch.chdir(world["author"])
    r = invoke("push", "union:alice/review", "--version", "2.0.0", "--config", world["config"])
    assert r.exit_code == 0, r.output
    monkeypatch.chdir(world["me"])
    p = invoke("pull", "union:alice/review@2.0.0", "--config", world["config"], "--as", "refresher")
    assert p.exit_code == 0, p.output
    lock = world["tmp"] / "hlock-home" / "pulls" / "refresher" / "harness.lock"
    assert "union:alice/review@2.0.0" in lock.read_text()
    # refresh it (no source given → walks pulled locks and refetches via union)
    m = invoke("pull", "--config", world["config"], "refresher")
    assert m.exit_code == 0, m.output
