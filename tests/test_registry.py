"""The registry client: `hlock login`, `logout`, `whoami`, `push`, `search`, and
`hlock pull NAMESPACE/NAME`, against an in-process registry that speaks the hlock registry
HTTP API v1 (specs/registry.md)."""

import gzip
import hashlib
import io
import json
import os
import re
import shutil
import stat
import subprocess
import tarfile
import threading
import zipfile
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs, urlparse

import pytest
from click.testing import CliRunner

from hlock import Lockfile, is_registry_ref
from hlock.cli import main

TOKEN = "hlk_test_0123456789"

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


def invoke(*args, input=None):
    return CliRunner().invoke(main, list(args), input=input)


class Registry:
    """A registry's state: packages by `ns/name`, each with versions and archives."""

    def __init__(self):
        self.packages: dict[str, dict] = {}
        self.archives: dict[tuple[str, str], bytes] = {}
        self.polls = 0
        self.tamper = False
        self.searches: list[dict] = []
        self.content_types: dict[tuple[str, str], str] = {}

    def add(self, package: str, version: str, archive: bytes, **meta):
        p = self.packages.setdefault(package, {"package": package, "type": "setup", "source": "hosted", "versions": []})
        p.update(meta)
        digest = "sha256:" + hashlib.sha256(archive).hexdigest()
        p["versions"].insert(0, {"version": version, "digest": digest, "published_at": "2026-10-22T00:00:00Z",
                                 "size_bytes": len(archive), "yanked": False})
        self.archives[(package, version)] = archive
        return digest


def handler_for(reg: Registry):
    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *a):
            pass

        def send(self, status, body=b"", ctype="application/json", headers=None):
            if isinstance(body, (dict, list)):
                body = json.dumps(body).encode()
            self.send_response(status)
            self.send_header("content-type", ctype)
            self.send_header("content-length", str(len(body)))
            # an HTTP/1.0 server closes every connection; saying so keeps the client from
            # reusing one it has already closed ("Peer disconnected")
            self.send_header("connection", "close")
            for k, v in (headers or {}).items():
                self.send_header(k, v)
            self.end_headers()
            self.wfile.write(body)

        def problem(self, status, title, detail="", errors=None):
            body = {"title": title, "status": status, "detail": detail}
            if errors:
                body["extensions"] = {"errors": errors}
            self.send(status, body, "application/problem+json")

        def authed(self):
            return self.headers.get("authorization") == f"Bearer {TOKEN}"

        def body(self) -> bytes:
            return self.rfile.read(int(self.headers.get("content-length") or 0))

        def do_POST(self):
            path = urlparse(self.path).path
            data = json.loads(self.body() or b"{}")
            if path == "/api/v1/auth/device":
                return self.send(200, {"device_code": "dc-1", "user_code": "WXYZ-1234", "verification_uri": "http://x/device",
                                       "verification_uri_complete": "http://x/device?code=WXYZ-1234", "expires_in": 60,
                                       "interval": 0})
            if path == "/api/v1/auth/device/token":
                if data.get("device_code") != "dc-1":
                    return self.send(400, {"error": "expired_token"})
                reg.polls += 1
                if reg.polls < 2:
                    return self.send(400, {"error": "authorization_pending"})
                return self.send(200, {"access_token": TOKEN, "token_type": "bearer", "scopes": ["read", "publish"],
                                       "user": {"handle": "alice"}})
            self.problem(404, "Not Found")

        def do_GET(self):
            u = urlparse(self.path)
            parts = u.path.removeprefix("/api/v1/").split("/")
            if u.path == "/api/v1/whoami":
                if not self.authed():
                    return self.problem(401, "Unauthorized", "missing or invalid token")
                return self.send(200, {"user": {"handle": "alice", "name": "Alice"}, "namespaces": ["alice"],
                                       "scopes": ["read", "publish"]})
            if u.path == "/api/v1/search":
                q = {k: v[0] for k, v in parse_qs(u.query).items()}
                reg.searches.append(q)
                results = [{"ref": p, "type": m["type"], "source": m["source"], "kinds": ["harness", "apm"],
                            "description": "a setup", "harness": "pi", "pull": m.get("pull", p)}
                           for p, m in reg.packages.items()]
                return self.send(200, {"total": len(results), "page": 1, "per_page": 20, "results": results})
            if parts[0] == "packages" and len(parts) >= 3:
                pkg = reg.packages.get(f"{parts[1]}/{parts[2]}")
                if pkg is None:
                    return self.problem(404, "Not Found", "no such package")
                if len(parts) == 3:
                    return self.send(200, pkg)
                if len(parts) == 4 and parts[3] == "versions":
                    return self.send(200, {"package": pkg["package"], "versions": pkg["versions"]})
                versions = [v for v in pkg["versions"] if not v["yanked"]]
                v = versions[0] if parts[4] == "latest" else next((x for x in pkg["versions"] if x["version"] == parts[4]), None)
                if v is None:
                    return self.problem(404, "Not Found", "no such version")
                if len(parts) == 5:
                    return self.send(200, {**v, "pull": pkg.get("pull")})
                data = reg.archives[(pkg["package"], v["version"])]
                if reg.tamper:
                    data = data[:-1] + bytes([data[-1] ^ 1])
                ctype = "application/zip" if data.startswith(b"PK") else "application/gzip"
                return self.send(200, data, ctype, {"x-hlock-digest": v["digest"]})
            self.problem(404, "Not Found")

        def do_PUT(self):
            parts = urlparse(self.path).path.removeprefix("/api/v1/").split("/")
            data = self.body()
            if not self.authed():
                return self.problem(401, "Unauthorized")
            if parts[1] != "alice":
                return self.problem(403, "Forbidden", "no publish scope on " + parts[1])
            package, version = f"{parts[1]}/{parts[2]}", parts[4]
            if (package, version) in reg.archives:
                return self.problem(409, "Conflict", "version exists")
            meta = {}
            if self.headers.get("content-type") == "application/zip":
                # an APM package, as APM's registry API checks it (§6)
                with zipfile.ZipFile(io.BytesIO(data)) as z:
                    names = z.namelist()
                    if "apm.yml" not in names:
                        return self.problem(422, "Unprocessable", errors=["no apm.yml at the root"])
                    if f"version: {version}" not in z.read("apm.yml").decode():
                        return self.problem(422, "Unprocessable", errors=["apm.yml's version doesn't match the URL"])
                meta["type"] = "apm-package"
            else:
                try:
                    with tarfile.open(fileobj=io.BytesIO(data), mode="r:gz") as t:
                        names = t.getnames()
                except tarfile.TarError:
                    return self.problem(422, "Unprocessable", errors=["not a tar.gz"])
            reg.content_types[(package, version)] = self.headers.get("content-type")
            digest = reg.add(package, version, data, **meta)
            self.send(201, {"package": package, "version": version, "digest": digest, "published_at": "2026-10-22T00:00:00Z",
                            "size_bytes": len(data), "files": names, "url": f"http://registry/{package}"})

    return Handler


@pytest.fixture
def world(tmp_path, monkeypatch):
    """A registry on localhost, an author's project with a setup in it, and a clean hlock home."""
    for key in list(os.environ):
        if key.startswith(("HLOCK_", "GIT_")):
            monkeypatch.delenv(key)
    monkeypatch.setenv("HLOCK_HOME", str(tmp_path / "hlock-home"))
    monkeypatch.setenv("HLOCK_KEYCHAIN", "0")
    monkeypatch.setenv("HOME", str(tmp_path / "home"))
    monkeypatch.setenv("NO_COLOR", "1")
    (tmp_path / "home").mkdir()
    reg = Registry()
    server = ThreadingHTTPServer(("127.0.0.1", 0), handler_for(reg))
    threading.Thread(target=server.serve_forever, daemon=True).start()
    url = f"http://127.0.0.1:{server.server_address[1]}"

    author = tmp_path / "author"
    (author / ".hlock/vendor/pi/skills/review").mkdir(parents=True)
    (author / ".hlock/vendor/pi/skills/review/SKILL.md").write_text("Review the diff.\n")
    (author / "harness.lock").write_text(LOCK)
    (author / "harness.toml").write_text('[project]\nname = "My Setup"\n')
    (author / "apm.yml").write_text("name: my-setup\nversion: 1.2.0\ndependencies:\n  apm: []\n")
    (author / "apm.lock.yaml").write_text('lockfile_version: "1"\ndependencies: []\n')
    (author / "notes.txt").write_text("not part of the setup")
    me = tmp_path / "me"
    me.mkdir()
    monkeypatch.chdir(me)
    yield {"url": url, "reg": reg, "author": author, "me": me, "home": tmp_path / "hlock-home", "tmp": tmp_path}
    server.shutdown()


def login(world):
    r = invoke("login", world["url"], "--token", TOKEN)
    assert r.exit_code == 0, r.output


def tarball(entries: dict[str, bytes]) -> bytes:
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w:gz") as t:
        for name, data in entries.items():
            info = tarfile.TarInfo(name)
            info.size = len(data)
            t.addfile(info, io.BytesIO(data))
    return buf.getvalue()


def test_registry_refs_are_recognized(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    assert is_registry_ref("acme/review-bot") and is_registry_ref("acme/review-bot@1.2.0")
    assert not is_registry_ref("github:acme/review-bot") and not is_registry_ref("review-bot")
    (tmp_path / "locks").mkdir()
    (tmp_path / "locks/harness.lock").write_text(LOCK)
    assert not is_registry_ref("locks/harness.lock")


def test_no_registry_configured(world):
    r = invoke("whoami")
    assert r.exit_code == 1 and "no registry configured" in r.output and "hlock login" in r.output


def test_login_with_the_device_flow(world):
    r = invoke("login", world["url"], "--no-browser")
    assert r.exit_code == 0, r.output
    assert "WXYZ-1234" in r.output and "logged in to" in r.output and "alice" in r.output
    assert world["reg"].polls == 2
    creds = world["home"] / "credentials.toml"
    assert stat.S_IMODE(creds.stat().st_mode) == 0o600
    assert f'[registries."{world["url"]}"]\ntoken = "{TOKEN}"' in creds.read_text()
    assert f'[registry]\nurl = "{world["url"]}"' in (world["home"] / "config.toml").read_text()
    r = invoke("whoami")
    assert r.exit_code == 0 and "alice" in r.output and "credentials file" in r.output


def test_login_with_a_token_logout_and_the_environment(world, monkeypatch):
    r = invoke("login", world["url"], "--token", "-", input=TOKEN + "\n")
    assert r.exit_code == 0, r.output
    r = invoke("whoami", "--json")
    assert json.loads(r.output)["user"]["handle"] == "alice"
    assert invoke("logout").exit_code == 0
    r = invoke("whoami")
    assert r.exit_code == 1 and "not logged in" in r.output
    # CI: the registry and the token from the environment
    monkeypatch.setenv("HLOCK_REGISTRY_TOKEN", TOKEN)
    monkeypatch.setenv("HLOCK_REGISTRY", world["url"])
    (world["home"] / "config.toml").unlink()
    r = invoke("whoami")
    assert r.exit_code == 0 and "environment" in r.output


def test_a_bad_token_is_refused(world):
    r = invoke("login", world["url"], "--token", "hlk_wrong")
    assert r.exit_code == 1 and "401" in r.output
    assert not (world["home"] / "credentials.toml").exists()


def test_push_dry_run_then_publish_once(world):
    r = invoke("push", "-C", str(world["author"]), "--dry-run", "--registry", world["url"])
    assert r.exit_code == 0, r.output
    assert "<your-namespace>/my-setup@1.2.0" in r.output
    for f in ("harness.lock", "harness.toml", "apm.yml", "apm.lock.yaml", "SKILL.md"):
        assert f in r.output
    assert "notes.txt" not in r.output
    login(world)
    r = invoke("push", "-C", str(world["author"]), "--json")
    out = json.loads(r.output)
    assert out["package"] == "alice/my-setup" and out["version"] == "1.2.0" and out["published"]
    files = [f["path"] for f in out["files"]]
    assert files == [".hlock/vendor/pi/skills/review/SKILL.md", "apm.lock.yaml", "apm.yml", "harness.lock", "harness.toml"]
    # the same files pack to the same digest: a second push is a no-op
    r = invoke("push", "-C", str(world["author"]))
    assert r.exit_code == 0 and "already published" in r.output
    # other files under the same version are refused
    (world["author"] / "README.md").write_text("# My setup\n")
    r = invoke("push", "-C", str(world["author"]))
    assert r.exit_code == 1 and "already published with other content" in r.output
    r = invoke("push", "-C", str(world["author"]), "--version", "1.2.1")
    assert r.exit_code == 0 and "pushed alice/my-setup@1.2.1" in r.output


def test_push_defaults_to_a_content_version_and_respects_namespaces(world):
    login(world)
    (world["author"] / "apm.yml").unlink()
    r = invoke("push", "-C", str(world["author"]), "--json")
    out = json.loads(r.output)
    assert re.fullmatch(r"sha-[0-9a-f]{12}", out["version"]) and out["digest"].startswith("sha256:" + out["version"][4:])
    r = invoke("push", "acme/my-setup", "-C", str(world["author"]))
    assert r.exit_code == 1 and "403" in r.output and "scopes" in r.output


def test_push_refuses_credentials_and_hidden_characters(world):
    login(world)
    (world["author"] / "harness.toml").write_text(
        '[project]\nname = "x"\n[env]\nOPENAI_API_KEY = "sk-proj-abcdefghijklmnopqrstuvwxyz0123456789"\n')
    r = invoke("push", "-C", str(world["author"]))
    assert r.exit_code == 1 and "credentials" in r.output and "OPENAI_API_KEY" in r.output
    (world["author"] / "harness.toml").write_text('[project]\nname = "x"\n')
    (world["author"] / ".hlock/vendor/pi/skills/review/SKILL.md").write_text("Review\u202e the diff.\n")
    r = invoke("push", "-C", str(world["author"]))
    assert r.exit_code == 1 and "U+202E" in r.output
    assert world["reg"].packages == {}


def test_search(world):
    login(world)
    invoke("push", "-C", str(world["author"]))
    r = invoke("search", "code", "review", "--kind", "apm", "--source", "hosted")
    assert r.exit_code == 0, r.output
    assert "alice/my-setup" in r.output and "hlock pull alice/my-setup" in r.output
    assert world["reg"].searches[-1] == {"q": "code review", "kind": "apm", "source": "hosted", "per_page": "20"}


def pulled_table(lock: Path) -> dict:
    return json.loads(Lockfile.load(str(lock)).to_json())["pulled"]


def published(world) -> None:
    login(world)
    assert invoke("push", "-C", str(world["author"])).exit_code == 0


def test_pull_from_the_registry_into_the_store_and_refresh(world):
    published(world)
    r = invoke("pull", "alice/my-setup")
    assert r.exit_code == 0, r.output
    assert "pulled alice/my-setup @ 1.2.0 → my-setup" in r.output
    stored = world["home"] / "pulls/my-setup"
    assert (stored / ".hlock/vendor/pi/skills/review/SKILL.md").read_text() == "Review the diff.\n"
    pulled = pulled_table(stored / "harness.lock")
    assert pulled["from"] == f"registry:{world['url']}/alice/my-setup"
    assert pulled["version"] == "1.2.0" and pulled["digest"].startswith("sha256:")
    r = invoke("list")
    assert "my-setup" in r.output and "1.2.0" in r.output
    r = invoke("pull")
    assert r.exit_code == 0 and "up to date" in r.output
    # a newer version is picked up by a refresh
    (world["author"] / "README.md").write_text("# new\n")
    invoke("push", "-C", str(world["author"]), "--version", "1.3.0")
    r = invoke("pull", "my-setup")
    assert r.exit_code == 0 and "1.2.0 → 1.3.0" in r.output


def test_pull_a_version_into_a_project_lock(world):
    published(world)
    r = invoke("pull", "alice/my-setup@1.2.0", "-o", "harness.alice.lock")
    assert r.exit_code == 0, r.output
    assert (world["me"] / "harness.alice.lock").is_file()
    assert (world["me"] / ".hlock/vendor/alice/pi/skills/review/SKILL.md").is_file()


def test_pull_into_writes_the_whole_setup(world):
    published(world)
    r = invoke("pull", "alice/my-setup", "--into", "new")
    assert r.exit_code == 0, r.output
    new = world["me"] / "new"
    for f in ("harness.lock", "harness.toml", "apm.yml", "apm.lock.yaml", ".hlock/vendor/pi/skills/review/SKILL.md"):
        assert (new / f).is_file(), f
    assert "apm install --frozen" in r.output
    assert pulled_table(new / "harness.lock")["version"] == "1.2.0"
    r = invoke("pull", "alice/my-setup", "--into", "new")
    assert r.exit_code == 1 and "already has" in r.output and "--force" in r.output
    assert invoke("pull", "alice/my-setup", "--into", "new", "--force").exit_code == 0


def test_an_archive_that_does_not_match_its_digest_is_refused(world):
    published(world)
    world["reg"].tamper = True
    r = invoke("pull", "alice/my-setup")
    assert r.exit_code == 1 and "doesn't match its digest" in r.output
    assert not (world["home"] / "pulls/my-setup").exists()


@pytest.mark.parametrize("entries,why", [
    ({"harness.lock": LOCK.encode(), "../../evil": b"x"}, "outside the archive"),
    ({"harness.lock": LOCK.encode(), "run.sh": b"rm -rf ~"}, "isn't a file a setup may contain"),
    ({"README.md": b"hi"}, "none of"),
])
def test_unsafe_archives_are_refused(world, entries, why):
    world["reg"].add("alice/evil", "1", tarball(entries))
    r = invoke("pull", "alice/evil", "--registry", world["url"])
    assert r.exit_code == 1 and why in r.output, r.output
    assert not (world["me"] / "evil").exists() and not (world["home"] / "pulls/evil").exists()


def test_an_apm_package_points_at_apm(world):
    world["reg"].add("alice/skills", "1.0.0", tarball({"apm.yml": b"name: skills\nversion: 1.0.0\n"}), type="apm-package")
    r = invoke("pull", "alice/skills", "--registry", world["url"])
    assert r.exit_code == 1 and "APM package" in r.output and "apm install alice/skills" in r.output
    assert "hlock apm pull alice/skills" in r.output


def git(repo: Path, *args: str) -> str:
    env = {**os.environ, "GIT_AUTHOR_NAME": "a", "GIT_AUTHOR_EMAIL": "a@x", "GIT_COMMITTER_NAME": "a",
           "GIT_COMMITTER_EMAIL": "a@x"}
    return subprocess.run(["git", *args], cwd=repo, env=env, check=True, capture_output=True, text=True).stdout.strip()


def test_a_remote_package_is_pulled_from_git(world, monkeypatch):
    remotes = world["tmp"] / "remotes"
    repo = remotes / "bob/setup.git"
    shutil.copytree(world["author"], repo)
    git(repo, "init", "-q", "-b", "main")
    git(repo, "add", "-A")
    git(repo, "commit", "-q", "-m", "pin")
    monkeypatch.setenv("HLOCK_GITHUB_URL", remotes.as_uri())
    world["reg"].packages["github/bob"] = {"package": "github/bob", "type": "setup", "source": "github",
                                           "pull": "github:bob/setup", "versions": []}
    r = invoke("pull", "github/bob", "--registry", world["url"], "--json")
    assert r.exit_code == 0, r.output
    out = json.loads(r.output)
    assert out["source"]["spec"] == "github:bob/setup" and out["registry"]["source"] == "github"
    assert (world["home"] / "pulls/bob/harness.lock").is_file()
    r = invoke("pull", "github:bob/setup", "--into", "from-git")
    assert r.exit_code == 0, r.output
    assert (world["me"] / "from-git/apm.lock.yaml").is_file() and (world["me"] / "from-git/harness.toml").is_file()


def test_gzip_bomb_guard_counts_unpacked_bytes(world):
    # a small archive that would unpack past the limit is refused before it's written
    big = tarball({"harness.lock": LOCK.encode(), "README.md": b"0" * (65 << 20)})
    assert len(gzip.compress(b"0" * 10)) < 100 and len(big) < (4 << 20)
    world["reg"].add("alice/bomb", "1", big)
    r = invoke("pull", "alice/bomb", "--registry", world["url"])
    assert r.exit_code == 1 and "MiB" in r.output


def test_json_output_stays_clean_when_a_terminal_is_forced(world, monkeypatch):
    # CI systems often set FORCE_COLOR; progress spinners must not land in --json output.
    from hlock import cli

    login(world)
    monkeypatch.setattr(cli.console, "_force_terminal", True)
    monkeypatch.setattr(cli.err_console, "_force_terminal", True)
    try:
        runner = CliRunner(mix_stderr=False)  # click < 8.2 mixes stderr into stdout by default
    except TypeError:
        runner = CliRunner()
    r = runner.invoke(main, ["whoami", "--json"])
    assert r.exit_code == 0, r.output
    assert json.loads(r.stdout)["user"]["handle"]


def apm_package(root: Path, version: str = "1.0.0") -> Path:
    pkg = root / "review-skills"
    (pkg / ".apm/skills/review").mkdir(parents=True, exist_ok=True)
    (pkg / ".apm/skills/review/SKILL.md").write_text("Review the diff.\n")
    (pkg / "apm.yml").write_text(f"name: review-skills\nversion: {version}\n")
    (pkg / "README.md").write_text("# Review skills\n")
    (pkg / "notes.txt").write_text("not part of the package")
    return pkg


def test_apm_push_publishes_a_zip_like_apm_publish(world):
    login(world)
    pkg = apm_package(world["tmp"])
    r = invoke("apm", "push", "-C", str(pkg), "--dry-run", "--json")
    assert r.exit_code == 0, r.output
    dry = json.loads(r.stdout)
    assert dry["package"] == "alice/review-skills" and dry["version"] == "1.0.0" and dry["dry_run"]
    assert [f["path"] for f in dry["files"]] == [".apm/skills/review/SKILL.md", "README.md", "apm.yml"]
    assert world["reg"].packages == {}

    r = invoke("apm", "push", "-C", str(pkg))
    assert r.exit_code == 0, r.output
    assert "pushed APM package alice/review-skills@1.0.0" in r.output
    assert f"{world['url']}/api" in r.output and "alice/review-skills#1.0.0" in r.output
    archive = world["reg"].archives[("alice/review-skills", "1.0.0")]
    assert world["reg"].content_types[("alice/review-skills", "1.0.0")] == "application/zip"
    with zipfile.ZipFile(io.BytesIO(archive)) as z:
        assert z.namelist() == [".apm/skills/review/SKILL.md", "README.md", "apm.yml"]
    assert "sha256:" + hashlib.sha256(archive).hexdigest() == dry["digest"]

    # the same files again are already published; changed files under the same version aren't allowed
    r = invoke("apm", "push", "-C", str(pkg))
    assert r.exit_code == 0 and "already published" in r.output, r.output
    (pkg / ".apm/skills/review/SKILL.md").write_text("Review it twice.\n")
    r = invoke("apm", "push", "-C", str(pkg))
    assert r.exit_code == 1 and "Bump `version` in apm.yml" in r.output


def test_apm_push_needs_a_package(world):
    login(world)
    r = invoke("apm", "push", "-C", str(world["author"]))
    assert r.exit_code == 1 and ".apm/" in r.output and "hlock push" in r.output
    r = invoke("apm", "push", "alice/x@2", "-C", str(apm_package(world["tmp"])))
    assert r.exit_code == 1 and "comes from apm.yml" in r.output


def test_apm_pull_writes_the_newest_version(world):
    login(world)
    pkg = apm_package(world["tmp"])
    assert invoke("apm", "push", "-C", str(pkg)).exit_code == 0
    (pkg / "apm.yml").write_text("name: review-skills\nversion: 1.10.0\n")
    assert invoke("apm", "push", "-C", str(pkg)).exit_code == 0

    r = invoke("apm", "pull", "alice/review-skills", "--json")
    assert r.exit_code == 0, r.output
    got = json.loads(r.stdout)
    assert got["version"] == "1.10.0" and got["install"] == "alice/review-skills#1.10.0"
    assert (world["me"] / "review-skills/apm.yml").read_text().endswith("1.10.0\n")
    assert (world["me"] / "review-skills/.apm/skills/review/SKILL.md").is_file()
    assert not (world["me"] / "review-skills/notes.txt").exists()

    # an older version, into a directory that has it already
    r = invoke("apm", "pull", "alice/review-skills@1.0.0")
    assert r.exit_code == 1 and "--force" in r.output
    r = invoke("apm", "pull", "alice/review-skills@1.0.0", "--force")
    assert r.exit_code == 0 and "@ 1.0.0" in r.output, r.output
    assert (world["me"] / "review-skills/apm.yml").read_text().endswith("1.0.0\n")
    r = invoke("apm", "pull", "alice/review-skills@9.9.9", "--into", "x")
    assert r.exit_code == 1 and "no version 9.9.9" in r.output and "1.10.0" in r.output


def test_apm_pull_checks_what_it_downloads(world):
    login(world)
    world["reg"].add("alice/tar", "1", tarball({"apm.yml": b"name: tar\nversion: 1\n", ".apm/x.md": b"hi\n"}),
                     type="apm-package")
    r = invoke("apm", "pull", "alice/tar")
    assert r.exit_code == 0, r.output
    assert (world["me"] / "tar/.apm/x.md").read_text() == "hi\n"

    world["reg"].add("alice/evil", "1", tarball({"apm.yml": b"name: evil\nversion: 1\n", "../escape": b"x"}))
    r = invoke("apm", "pull", "alice/evil")
    assert r.exit_code == 1 and "outside the archive" in r.output
    assert not (world["me"] / "escape").exists() and not (world["me"] / "evil").exists()

    world["reg"].add("alice/hidden", "1", tarball({"apm.yml": b"name: hidden\nversion: 1\n",
                                                   ".apm/x.md": "hi\u202ethere\n".encode()}))
    r = invoke("apm", "pull", "alice/hidden")
    assert r.exit_code == 1 and "--allow-hidden" in r.output and not (world["me"] / "hidden").exists()
    assert invoke("apm", "pull", "alice/hidden", "--allow-hidden", ".apm/x.md").exit_code == 0

    world["reg"].add("alice/review-bot", "1.0.0", tarball({"harness.lock": LOCK.encode()}))
    r = invoke("apm", "pull", "alice/review-bot")
    assert r.exit_code == 1 and "hlock pull alice/review-bot" in r.output

    pkg = apm_package(world["tmp"])
    assert invoke("apm", "push", "-C", str(pkg)).exit_code == 0
    world["reg"].tamper = True
    r = invoke("apm", "pull", "alice/review-skills")
    assert r.exit_code == 1 and "doesn't match its digest" in r.output
