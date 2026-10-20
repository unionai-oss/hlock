"""Record the docs home page's animated terminal as a video, for the README.

    uv run --group docs zensical build --clean
    uv run --with playwright python scripts/demo_video.py   # writes docs/assets/demo.{webp,mp4}

Plays the first harness's round of docs/javascripts/terminal.js (pin and push on one
machine, then pull and sync on another) in headless Chrome under a fake clock, one
screenshot per frame, so the recording is smooth and the same every time. The README shows
the animated WebP; the MP4 is for sharing. Needs Google Chrome and ffmpeg.
"""

from __future__ import annotations

import functools
import http.server
import subprocess
import tempfile
import threading
from pathlib import Path

from playwright.sync_api import sync_playwright

REPO = Path(__file__).resolve().parent.parent
SITE = REPO / "site"
ASSETS = REPO / "docs" / "assets"
FPS = 30
WIDTH = 900  # CSS pixels of the terminal window
SCALE = 2  # device pixels per CSS pixel: both files keep this resolution, sharp on HiDPI screens
BACKGROUND = "0x0c0a14"  # behind the window while it swipes, in the MP4

# A seeded Math.random, so the typing rhythm is the same on every run; and a hook that
# drives CSS transitions from the fake clock, which doesn't control them otherwise.
INIT = """
(() => {
  let s = 1;
  Math.random = () => {
    s = (s + 0x6d2b79f5) | 0;
    let t = Math.imul(s ^ (s >>> 15), 1 | s);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
  const started = new WeakMap();
  // A transition seen for the first time started during the last frame, at `prev`.
  window.__step = (now, prev) => {
    for (const a of document.getAnimations()) {
      if (!started.has(a)) { started.set(a, prev); a.pause(); }
      a.currentTime = now - started.get(a);
    }
  };
})();
"""


def serve(root: Path) -> tuple[http.server.ThreadingHTTPServer, str]:
    class Quiet(http.server.SimpleHTTPRequestHandler):
        def log_message(self, *_args) -> None:
            pass

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), functools.partial(Quiet, directory=str(root)))
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server, f"http://127.0.0.1:{server.server_address[1]}/"


def one_harness(route) -> None:
    """Serve terminal.js with only its first harness, so the video is one round that loops."""
    js = (SITE / "javascripts" / "terminal.js").read_text()
    js = js.replace("harnesses.flatMap(scenesFor)", "harnesses.slice(0, 1).flatMap(scenesFor)")
    route.fulfill(body=js, content_type="text/javascript")


def record(frames: Path) -> int:
    server, url = serve(SITE)
    try:
        with sync_playwright() as p:
            browser = p.chromium.launch(channel="chrome")
            page = browser.new_page(viewport={"width": WIDTH + 200, "height": 900}, device_scale_factor=SCALE,
                                    color_scheme="dark", reduced_motion="no-preference")
            page.add_init_script(INIT)
            page.route("**/javascripts/terminal.js", one_harness)
            # Frozen from the start: only run_for moves the clock, not page loads or screenshots.
            page.clock.install(time=0)
            page.clock.pause_at(1000)
            page.goto(url)
            page.add_style_tag(content=f"""
                html, body {{ background: transparent !important; }}
                .hl-demo {{ position: fixed; inset: 0 auto auto 0; width: {WIDTH}px; margin: 0;
                            padding: 24px; z-index: 1000; }}
                .hl-term {{ box-shadow: none !important; }}
                body * {{ visibility: hidden; }}
                .hl-demo, .hl-demo * {{ visibility: visible; }}
            """)
            page.evaluate("document.fonts.ready")
            box = page.locator(".hl-demo").bounding_box()
            clip = {"x": box["x"] + 24, "y": box["y"] + 24, "width": box["width"] - 48, "height": box["height"] - 48}
            term = page.locator("#hl-term")
            # The round ends when the second swipe (back to the first scene) has finished and
            # the first command's prompt appears again: that frame is the video's first, so
            # stop before it and the video loops seamlessly.
            body = page.locator(".hl-term__body")
            swipes, n, ms = 0, 0, 1000 / FPS
            leaving = False
            while True:
                if n:
                    page.clock.run_for(round(n * ms) - round((n - 1) * ms))
                page.evaluate(f"window.__step({n * ms}, {max(n - 1, 0) * ms})")
                now_leaving = "is-leaving" in (term.get_attribute("class") or "")
                swipes += now_leaving and not leaving
                leaving = now_leaving
                if swipes == 2 and not leaving and body.inner_text().startswith("$"):
                    break
                page.screenshot(path=str(frames / f"{n:05d}.png"), clip=clip, omit_background=True)
                n += 1
            browser.close()
    finally:
        server.shutdown()
    return n


def encode(frames: Path) -> None:
    src = ["-framerate", str(FPS), "-i", str(frames / "%05d.png")]
    mp4 = ASSETS / "demo.mp4"
    webp = ASSETS / "demo.webp"
    subprocess.run(["ffmpeg", "-y", "-loglevel", "error", "-f", "lavfi", "-i", f"color=c={BACKGROUND}", *src,
                    "-filter_complex", "[0][1]scale2ref[bg][fg];[bg][fg]overlay=shortest=1,crop=floor(iw/2)*2:floor(ih/2)*2,format=yuv420p",
                    "-c:v", "libx264", "-crf", "18", "-preset", "slow", "-tune", "animation", "-movflags", "+faststart", str(mp4)], check=True)
    subprocess.run(["ffmpeg", "-y", "-loglevel", "error", *src, "-vf", "fps=20",
                    "-c:v", "libwebp_anim", "-quality", "85", "-preset", "text", "-compression_level", "6", "-loop", "0", str(webp)], check=True)
    for f in (webp, mp4):
        print(f"wrote {f.relative_to(REPO)}  ({f.stat().st_size / 1e6:.1f} MB)")


def main() -> None:
    if not (SITE / "index.html").is_file():
        raise SystemExit("build the docs first: uv run --group docs zensical build --clean")
    with tempfile.TemporaryDirectory() as tmp:
        frames = Path(tmp)
        n = record(frames)
        print(f"recorded {n} frames ({n / FPS:.1f}s)")
        encode(frames)


if __name__ == "__main__":
    main()
