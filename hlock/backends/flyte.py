"""The ``flyte`` backend: each work item runs as a Flyte 2 task on a cluster.

It runs hlock's stock handlers (:func:`hlock.backends.run_local`) inside the task, so
results are the same as on ``local``; only where they run changes. CPU work items share
one task environment and ``ft.lora`` gets a GPU one. Tasks are cached (``cache="auto"``),
so a re-run reuses finished items, and each item's run is recorded in the tune lock's
provenance.

The tasks run the ``hlock`` installed in their image, never a bundle of local code. The
image installs ``HLOCK_FLYTE_HLOCK``: a pip requirement, or the path of a Linux wheel
(for an unreleased build); by default, ``hlock`` at this version.

Options (``[backends.flyte] options``): ``config`` is the Flyte config file (default:
the SDK's, ``~/.flyte/config.yaml``), ``project`` and ``domain`` override the ones in it,
``gpu`` is the accelerator for fine-tuning (default ``"A10G:1"``), and
``api_key_secret`` names the Flyte secret holding the model endpoint's key.

The tasks get ``HL_LLM_BASE_URL`` and ``HL_LLM_MODEL`` from this machine, in each
request, so the endpoint has to be reachable from the cluster. Tasks are cached on their
inputs, which include those and the hlock build, so a different endpoint or build runs
again. ``HL_LLM_API_KEY`` isn't sent: the tasks read it from
the ``api_key_secret`` Flyte secret.

A work item's ``workdir`` is on this machine and the task runs in a pod, so its files go
with the request and the files the task writes come back with the result (up to
``MAX_WORKSPACE_BYTES``), and hlock grades them here as on any backend.

If the Flyte SDK isn't installed, the item runs in process, as on ``local``, and says so
on stderr. A run that fails on the cluster fails the item with the run's error.
"""

from __future__ import annotations

import base64
import hashlib
import os
import sys
import tempfile
from pathlib import Path

from hlock.backends import Backend, BackendError, run_local

try:
    import flyte
except ImportError:  # the `flyte` extra isn't installed: items run in process
    flyte = None

GPU_KINDS = ("ft.lora",)
#: The most a work item's files may add up to, each way (task I/O is passed inline).
MAX_WORKSPACE_BYTES = 8 * 1024 * 1024
#: The contract's request and response files, which stay on this machine.
_CONTRACT_FILES = {"_al_request.json", "_al_response.json"}


def _pack(root: Path, skip: dict | None = None) -> dict:
    """The files under `root` as {relative path: base64}, less those unchanged from `skip`."""
    files, total = {}, 0
    if not root.is_dir():
        return files
    for p in sorted(root.rglob("*")):
        if not p.is_file() or p.is_symlink() or p.name in _CONTRACT_FILES:
            continue
        rel = p.relative_to(root).as_posix()
        data = base64.b64encode(p.read_bytes()).decode()
        if skip and skip.get(rel) == data:
            continue
        total += len(data)
        if total > MAX_WORKSPACE_BYTES:
            raise BackendError(f"the work item's files in {root} are over {MAX_WORKSPACE_BYTES} bytes, too many to send to Flyte")
        files[rel] = data
    return files


def _unpack(files: dict, root: Path) -> None:
    for rel, data in files.items():
        out = (root / rel).resolve()
        if root.resolve() not in out.parents:
            raise BackendError(f"a Flyte task returned a file outside its workspace: {rel}")
        out.parent.mkdir(parents=True, exist_ok=True)
        if out.is_symlink():
            out.unlink()
        out.write_bytes(base64.b64decode(data))


#: What the tasks get from this machine's environment (the key goes as a Flyte secret).
ENDPOINT_ENV = ("HL_LLM_BASE_URL", "HL_LLM_MODEL")


def _run_in_pod(request: dict) -> dict:
    """Run a work item in a scratch workspace holding the files sent with it, and return
    its result with the files it wrote."""
    sent = request.pop("files", {})
    # the endpoint travels in the request, so it's part of what the task cache keys on
    for k, v in (request.pop("env", None) or {}).items():
        if k in ENDPOINT_ENV:
            os.environ[k] = v
    request.pop("hlock", None)
    with tempfile.TemporaryDirectory() as ws:
        _unpack(sent, Path(ws))
        result = run_local({**request, "workdir": ws})
        result["files"] = _pack(Path(ws), skip=sent)
    return result
WHEEL_DIR = "/opt/hlock-wheel/"  # a directory: COPY to a path without the slash names the file


def _version() -> str:
    try:
        from hlock import __version__

        return __version__
    except Exception:  # noqa: BLE001
        return ""


def _hlock_spec() -> str:
    """What the task images install: `HLOCK_FLYTE_HLOCK`, else hlock at this version."""
    return os.environ.get("HLOCK_FLYTE_HLOCK") or (f"hlock=={_version()}" if _version() else "hlock")


def _task_version() -> str:
    """The registered tasks' version: hlock's, plus the wheel's digest for a local build."""
    spec = _hlock_spec()
    tag = spec
    if spec.endswith(".whl") and Path(spec).is_file():
        tag = hashlib.sha256(Path(spec).read_bytes()).hexdigest()
    return f"{_version() or 'dev'}-{hashlib.sha256(tag.encode()).hexdigest()[:12]}"


def _image(gpu: bool):
    pkgs = ["torch", "transformers", "datasets", "peft", "trl", "accelerate"] if gpu else ["pandas", "polars"]
    spec = _hlock_spec()
    wheel = Path(spec) if spec.endswith(".whl") and Path(spec).is_file() else None
    # a wheel is built for one platform, so the image is too
    platform = None
    if wheel is not None:
        platform = ("linux/arm64",) if "aarch64" in wheel.name else ("linux/amd64",) if "x86_64" in wheel.name else None
    image = flyte.Image.from_debian_base(python_version=(3, 12), name="hlock-gpu" if gpu else "hlock", platform=platform)
    if wheel is not None:
        image = image.with_source_file(wheel, WHEEL_DIR)
        spec = WHEEL_DIR + wheel.name
    return image.with_pip_packages(spec, *pkgs)


if flyte is not None:
    cpu_env = flyte.TaskEnvironment(name="hlock-cpu", image=_image(gpu=False), resources=flyte.Resources(cpu=4, memory="8Gi"))
    gpu_env = flyte.TaskEnvironment(name="hlock-gpu", image=_image(gpu=True), resources=flyte.Resources(cpu=8, memory="32Gi", gpu="A10G:1"))

    @cpu_env.task(cache="auto")
    async def run_item(request: dict) -> dict:
        """One hlock work item on CPU."""
        return _run_in_pod(request)

    @gpu_env.task(cache="auto")
    async def run_gpu_item(request: dict) -> dict:
        """One hlock work item on a GPU."""
        return _run_in_pod(request)


def _gpu_task(gpu: str):
    """The GPU task, on the accelerator `gpu` names."""
    if gpu == "A10G:1":
        return run_gpu_item
    return run_gpu_item.override(resources=flyte.Resources(cpu=8, memory="32Gi", gpu=gpu))


class FlyteBackend(Backend):
    name = "flyte"
    about = "Flyte 2 tasks on the cluster in your Flyte config"

    def run(self, request: dict) -> dict:
        if flyte is None:
            sys.stderr.write(f"[flyte] the Flyte SDK isn't installed (pip install \"hlock[flyte]\"); "
                             f"running {request.get('kind')} in process\n")
            return run_local(request)
        workdir = Path(request.get("workdir") or ".")
        result, run = self._lift({**request, "files": _pack(workdir)})
        _unpack(result.pop("files", None) or {}, workdir)
        digests = result.setdefault("model_digests", {})
        if isinstance(digests, dict):
            digests["run"] = getattr(run, "url", None) or getattr(run, "name", "flyte://unknown")
            digests["backend"] = "flyte"
        return result

    def _lift(self, request: dict):
        opts = request.get("backend_options") or {}
        scope = {k: opts[k] for k in ("project", "domain") if opts.get(k)}
        try:
            config = os.path.expanduser(opts["config"]) if opts.get("config") else None
            # tasks are named by their module, relative to the directory holding `hlock`
            flyte.init_from_config(config, root_dir=Path(__file__).resolve().parents[2], **scope)
        except Exception as exc:
            raise BackendError(f"can't connect to Flyte with {opts.get('config') or 'the default config'}: {exc}") from exc
        task = _gpu_task(opts.get("gpu") or "A10G:1") if request.get("kind") in GPU_KINDS else run_item
        if opts.get("api_key_secret"):
            task = task.override(secrets=flyte.Secret(key=opts["api_key_secret"], as_env_var="HL_LLM_API_KEY"))
        elif os.environ.get("HL_LLM_API_KEY"):
            raise BackendError("HL_LLM_API_KEY is set, but Flyte tasks don't get it from this machine: store it as a "
                               "Flyte secret and name it in [backends.flyte] options.api_key_secret")
        # everything that decides the result is in the inputs, which the task cache keys on:
        # the endpoint, and the hlock build the tasks run
        request = {**request, "env": {k: os.environ[k] for k in ENDPOINT_ENV if os.environ.get(k)}, "hlock": _task_version()}
        # the tasks are in the image's hlock, so nothing local is bundled
        run = flyte.with_runcontext(copy_style="none", version=_task_version(), **scope).run(task, request=request)
        run.wait()
        try:
            run.raise_for_status()
        except Exception as exc:
            raise BackendError(f"the Flyte run {getattr(run, 'url', '') or getattr(run, 'name', '')} failed: {exc}") from exc
        outputs = run.outputs()
        result = outputs[0] if isinstance(outputs, (list, tuple)) else outputs
        if not isinstance(result, dict):
            raise BackendError(f"the Flyte run {getattr(run, 'name', '')} returned no result")
        return result, run
