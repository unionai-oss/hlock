"""Running a locked agent in its sandbox (``specs/sandbox.md``).

The Rust core decides what a sandboxed run looks like: the runner's argv, its
environment, and how to stop it (``LaunchPrep.sandbox``). This module does what has to
happen in a live process around it: the provider-key proxy, starting and waiting on the
runner, stopping a container hlock was interrupted out of, and the checks before and
after (the image's labels, the control files a session changed).
"""

from __future__ import annotations

import hmac
import http.client
import json
import os
import platform
import secrets
import signal
import ssl
import subprocess
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import urlsplit

# route → (upstream, the header the real key goes in, the paths the route serves)
ROUTES: dict[str, tuple[str, str, tuple[str, ...]]] = {
    "anthropic": ("https://api.anthropic.com", "x-api-key", ("/v1/messages", "/v1/messages/count_tokens", "/v1/models")),
    "openai": ("https://api.openai.com", "authorization", ("/v1/chat/completions", "/v1/responses", "/v1/models", "/v1/embeddings")),
}
# the variable holding each route's key, and the base-URL variable the harness reads
KEY_VARS = {"anthropic": ("ANTHROPIC_API_KEY", "ANTHROPIC_BASE_URL"), "openai": ("OPENAI_API_KEY", "OPENAI_BASE_URL")}
HOP_BY_HOP = {"connection", "keep-alive", "proxy-authenticate", "proxy-authorization", "te", "trailer",
              "transfer-encoding", "upgrade", "host", "content-length"}


def _credential_header(name: str) -> bool:
    n = name.lower()
    return n in ("x-api-key", "authorization") or n.endswith(("-key", "-token"))


class Proxy:
    """A credential proxy for one sandboxed run: the sandbox gets a session token and a
    base URL; the proxy swaps the token for the real key and forwards to the provider.

    ``keys`` maps a route (``anthropic``, ``openai``) to its real key. ``upstreams``
    replaces a route's upstream URL, for tests only.
    """

    def __init__(self, keys: dict[str, str], host: str = "127.0.0.1", upstreams: dict[str, str] | None = None,
                 log: Path | None = None):
        unknown = set(keys) - set(ROUTES)
        if unknown:
            raise ValueError(f"no proxy route for {', '.join(sorted(unknown))}")
        self.keys = dict(keys)
        self.token = secrets.token_hex(32)
        self.upstreams = {r: (upstreams or {}).get(r, ROUTES[r][0]) for r in keys}
        self.log = log
        self._lock = threading.Lock()
        proxy = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, fmt, *args):  # never headers or bodies: method, path, status
                proxy._log(f"{self.command} {self.path.split('?', 1)[0]!r} {args[1] if len(args) > 1 else ''}")

            def do_CONNECT(self):
                self.send_error(405, "CONNECT isn't allowed")

            def _handle(self):
                proxy._forward(self)

            do_GET = do_POST = do_PUT = do_DELETE = do_PATCH = do_HEAD = do_OPTIONS = _handle

        self.server = ThreadingHTTPServer((host, 0), Handler)
        self.server.daemon_threads = True
        self.port = self.server.server_address[1]
        self._thread = threading.Thread(target=self.server.serve_forever, name="hlock-proxy", daemon=True)

    def start(self) -> Proxy:
        self._thread.start()
        return self

    def stop(self) -> None:
        self.server.shutdown()
        self.server.server_close()

    def env(self, host: str = "host.docker.internal") -> dict[str, str]:
        """What the sandbox gets: each route's base URL and the session token as its key."""
        out = {}
        for route in self.keys:
            key_var, base_var = KEY_VARS[route]
            out[key_var] = self.token
            out[base_var] = f"http://{host}:{self.port}/{route}"
        return out

    def _log(self, line: str) -> None:
        if not self.log:
            return
        # a new file of hlock's own, never through a link someone planted
        with self._lock:
            fd = os.open(self.log, os.O_WRONLY | os.O_APPEND | os.O_CREAT | getattr(os, "O_NOFOLLOW", 0), 0o600)
            with os.fdopen(fd, "a") as f:
                f.write(line + "\n")

    def _authorized(self, h: BaseHTTPRequestHandler) -> bool:
        given = h.headers.get("x-api-key") or ""
        auth = h.headers.get("authorization") or ""
        if auth.lower().startswith("bearer "):
            given = given or auth[7:].strip()
        return hmac.compare_digest(given.encode(), self.token.encode())

    def _forward(self, h: BaseHTTPRequestHandler) -> None:
        target = h.path
        # origin-form only: an absolute URI or a Host header never picks the upstream
        if not target.startswith("/"):
            return h.send_error(400, "absolute request targets aren't allowed")
        path, _, query = target.partition("?")
        low = path.lower()
        if ".." in path or "//" in path or "%2f" in low or "%5c" in low or "%2e" in low or "\\" in path:
            return h.send_error(400, "that path isn't allowed")
        route, _, rest = path[1:].partition("/")
        if route not in self.keys:
            return h.send_error(404, "no such route")
        rest = "/" + rest
        if rest not in ROUTES[route][2]:
            return h.send_error(403, f"{rest} isn't a path the proxy serves")
        if not self._authorized(h):
            return h.send_error(401, "bad session token")
        if "chunked" in (h.headers.get("transfer-encoding") or "").lower():
            return h.send_error(411, "send a Content-Length")
        n = int(h.headers.get("content-length") or 0)
        body = h.rfile.read(n) if n else None
        headers = {k: v for k, v in h.headers.items() if k.lower() not in HOP_BY_HOP and not _credential_header(k)}
        key_header = ROUTES[route][1]
        headers[key_header] = self.keys[route] if key_header == "x-api-key" else f"Bearer {self.keys[route]}"
        up = urlsplit(self.upstreams[route])
        conn: http.client.HTTPConnection
        if up.scheme == "https":
            conn = http.client.HTTPSConnection(up.hostname, up.port or 443, context=ssl.create_default_context(), timeout=600)
        else:
            conn = http.client.HTTPConnection(up.hostname, up.port or 80, timeout=600)
        try:
            conn.request(h.command, (up.path.rstrip("/") + rest) + (f"?{query}" if query else ""), body=body, headers=headers)
            resp = conn.getresponse()  # redirects aren't followed: http.client never does
            h.send_response(resp.status, resp.reason)
            length = resp.getheader("content-length")
            for k, v in resp.getheaders():
                if k.lower() not in HOP_BY_HOP:
                    h.send_header(k, v)
            if length is not None:
                h.send_header("Content-Length", length)
            h.send_header("Connection", "close")
            h.end_headers()
            h.close_connection = True
            # streamed as it arrives, so server-sent events reach the agent live
            while chunk := resp.read1(65536) if hasattr(resp, "read1") else resp.read(65536):
                h.wfile.write(chunk)
                h.wfile.flush()
        except OSError as e:
            try:
                h.send_error(502, f"the provider couldn't be reached: {type(e).__name__}")
            except OSError:
                pass
        finally:
            conn.close()


def proxy_keys(env: dict[str, str] | None = None) -> dict[str, str]:
    """The provider keys to proxy, from the environment."""
    env = os.environ if env is None else env
    return {route: env[k] for route, (k, _) in KEY_VARS.items() if env.get(k)}


def bind_address() -> str:
    """Where the proxy listens so a container reaches it: the host's loopback on Docker
    Desktop (macOS, Windows), the `bridge` network's gateway on Linux."""
    if platform.system() != "Linux":
        return "127.0.0.1"
    out = subprocess.run(["docker", "network", "inspect", "bridge", "--format", "{{(index .IPAM.Config 0).Gateway}}"],
                         capture_output=True, text=True, check=False)
    gw = out.stdout.strip()
    if out.returncode != 0 or not gw:
        raise RuntimeError("can't find the Docker bridge's gateway for the key proxy; use [sandbox] secrets = \"env\"")
    return gw


def approve_claude_key(root: str | os.PathLike, token: str) -> None:
    """Record the session token as an approved custom API key in Claude Code's state in the
    sandbox root, so an interactive session doesn't ask about it every time."""
    state = Path(root) / ".claude.json"
    if state.is_symlink():
        return  # the sandbox can write here: never read or write through a link
    try:
        doc = json.loads(state.read_text()) if state.is_file() else {}
    except (OSError, ValueError):
        return
    resp = doc.setdefault("customApiKeyResponses", {})
    approved = [k for k in resp.get("approved", []) if isinstance(k, str)][-20:]
    if token[-20:] not in approved:
        approved.append(token[-20:])
    resp["approved"] = approved
    fd, tmp = tempfile.mkstemp(dir=state.parent, prefix=".claude.json.")  # a new file, 0600
    with os.fdopen(fd, "w") as f:
        json.dump(doc, f, indent=2)
    os.replace(tmp, state)


def image_labels(image: str) -> dict[str, str] | None:
    """The image's labels, or None when it isn't here (not pulled yet) or Docker can't say."""
    try:
        out = subprocess.run(["docker", "image", "inspect", "--format", "{{json .Config.Labels}}", image],
                             capture_output=True, text=True, check=False, timeout=30)
    except (OSError, subprocess.TimeoutExpired):
        return None
    if out.returncode != 0:
        return None
    try:
        return json.loads(out.stdout or "null") or {}
    except ValueError:
        return None


def check_image(sb: dict, harness: str) -> list[str]:
    """Notes about the image's labels against the lock (empty when they agree); raises
    when they name another harness or version."""
    image = sb.get("image")
    if not image:
        return []
    labels = image_labels(image)
    if labels is None:
        return [f"{image} isn't on this machine yet; Docker pulls it, and its harness version isn't checked"]
    notes = []
    if "@sha256:" not in image:
        notes.append(f"{image} isn't pinned by digest")
    got, version = labels.get("ai.hlock.harness"), labels.get("ai.hlock.harness-version")
    if got is None:
        return notes + [f"{image} has no ai.hlock.harness label, so its harness version isn't checked"]
    if got != harness:
        raise RuntimeError(f"{image} is labeled for {got}, not {harness}")
    want = sb.get("harness_version")
    if want and version and version != want:
        raise RuntimeError(f"{image} has {harness} {version}; the lock pins {want} (rebuild it: hlock sync --docker --build)")
    return notes


def run(sb: dict, *, cwd: str | os.PathLike | None = None, stdout=None, stdin=None, timeout: float | None = None,
        interactive: bool = False) -> tuple[int | None, bool]:
    """Start the wrapped command and wait for it. Returns its exit code (None when it
    timed out) and whether it timed out. A timeout or an interrupt also runs the runner's
    kill command, since stopping `docker run` leaves its container running."""
    kill = sb.get("kill") or []
    env = sb["env"]

    def stop():
        if kill:
            subprocess.run(kill, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False)

    kwargs: dict = {"cwd": cwd, "env": env}
    if not interactive:
        kwargs.update(stdin=stdin if stdin is not None else subprocess.DEVNULL, stdout=stdout,
                      stderr=subprocess.STDOUT if stdout is not None else None, start_new_session=True)
    proc = subprocess.Popen(sb["argv"], **kwargs)
    # the session gets ^C from the terminal; hlock waits it out, then cleans up
    old = signal.signal(signal.SIGINT, signal.SIG_IGN) if interactive else None
    try:
        return proc.wait(timeout=timeout), False
    except subprocess.TimeoutExpired:
        try:
            os.killpg(proc.pid, signal.SIGKILL)
        except OSError:
            proc.kill()
        proc.wait()
        stop()
        return None, True
    except BaseException:
        proc.kill()
        proc.wait()
        stop()
        raise
    finally:
        if old is not None:
            signal.signal(signal.SIGINT, old)
