// Home page demo: an animated terminal that pins a setup and pushes it on one machine, then
// swipes to another machine that pulls and syncs it. Each round uses another harness; loops.
// Adapted from the hero terminal on hlock.ai (unionai-oss/hlock-website, public/main.js).
(() => {
  "use strict";

  const term = document.getElementById("hl-term");
  if (!term) return;

  const reduced = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

  // Markup helpers for terminal output.
  const span = (cls, text) => `<span class="${cls}">${text}</span>`;
  const ok = (t) => span("t-ok", t);
  const warn = (t) => span("t-warn", t);
  const auto = (t) => span("t-auto", t);
  const dim = (t) => span("t-dim", t);
  const mut = (t) => span("t-mut", t);
  const key = (t) => span("t-key", t);
  const hi = (t) => span("t-hi", t);
  const arrow = (t) => span("t-arrow", t);
  const plainLen = (html) => html.replace(/<[^>]+>/g, "").length;
  const escape = (s) => s.replace(/&/g, "&amp;").replace(/</g, "&lt;");

  // A rounded box like hlock's rich output; pads each row to the widest one.
  function box(title, rows) {
    const inner = Math.max(plainLen(title) + 6, ...rows.map((r) => plainLen(r) + 4));
    const b = (t) => span("t-box", t);
    const top = b("╭─ ") + title + " " + b("─".repeat(inner - plainLen(title) - 3) + "╮");
    const pad = (r) => b("│") + "  " + r + " ".repeat(inner - plainLen(r) - 2) + b("│");
    const blank = b("│") + " ".repeat(inner) + b("│");
    const bottom = b("╰" + "─".repeat(inner) + "╯");
    return [top, blank, ...rows.map(pad), blank, bottom];
  }
  const kv = (k, v) => key(k.padEnd(15)) + v;
  const plan = (kind, title) => kind.padEnd(9) + title;

  // The harnesses the demo cycles through, one per pin → sync round. Paths follow each
  // harness's profile (crates/harness-lock-core/profiles/harnesses/).
  const harnesses = [
    {
      id: "claude-code", name: "Claude Code", version: "2.1.285", llm: "anthropic:opus", root: "~/.claude",
      config: "{root}/settings.json, ~/.claude.json /mcpServers",
      writes: ["~/.claude/settings.json", "~/.claude.json /mcpServers"],
    },
    {
      id: "pi", name: "Pi", version: "1.0.0", llm: "openrouter:qwen/qwen3-coder", root: "~/.pi/agent",
      config: "{root}/settings.json, {root}/mcp.json",
      writes: ["~/.pi/agent/settings.json", "~/.pi/agent/mcp.json"],
    },
    {
      id: "codex", name: "Codex CLI", version: "0.50.0", llm: "openai:gpt-5-codex", root: "~/.codex",
      config: "{root}/config.toml",
      writes: ["~/.codex/config.toml"],
    },
    {
      id: "opencode", name: "OpenCode", version: "1.18.18", llm: "anthropic:claude-sonnet-4-5", root: "~/.config/opencode",
      config: "{root}/opencode.json",
      writes: ["~/.config/opencode/opencode.json"],
    },
    {
      id: "hermes", name: "Hermes Agent", version: "0.20.0", llm: "nous:hermes-4-405b", root: "~/.hermes",
      config: "{root}/config.yaml",
      writes: ["~/.hermes/config.yaml"],
    },
  ];
  const skills = ["review-pr", "write-tests"];

  // Two scenes per harness, each a terminal on one machine. { cmd } is typed; { out } lines
  // appear one at a time; { wait } pauses. The terminal swipes to the next scene.
  function scenesFor(h) {
    const setup = [
      kv("harness", `${h.id} ${h.version}`),
      kv("scope", "project + user"),
      kv("llm · default", h.llm),
      kv("mcp", "github (stdio), linear (http)"),
      kv("skills", "2  (2 local)"),
      kv("config", h.config),
      kv("secrets", "1 redacted  " + dim("(supplied as HLOCK_SECRET_* when syncing)")),
    ];
    const copies = skills.map((sk) => `Copy skill ${sk} → ${h.root}/skills/${sk}`);
    const done = [...h.writes.map((w) => "Write " + w), ...copies];
    return [
      {
        title: "laptop — ~/projects/my-app",
        caption: `On your laptop: pin your ${h.name} setup into harness.lock, then push it to GitHub.`,
        steps: [
          { cmd: "hlock pin --user" },
          { out: [ok("✓") + " pinned " + hi(`${h.id} ${h.version}`) + " → harness.lock  " + dim("(running inside it)")] },
          { out: box(hi("harness.lock · pinned"), setup), gap: 45 },
          { out: [mut(`vendored ${done.length} files into .hlock/vendor/${h.id}/  · review before committing`)] },
          { out: [arrow("next →") + " hlock check  ·  hlock sync"] },
          { wait: 900 },
          { cmd: 'git add harness.lock .hlock/ && git commit -qm "lock my agent"' },
          { cmd: "git push" },
          { out: [dim("To ") + hi("github.com:you/my-app.git"), dim("   b259f53..4e1c9a2  main -> main")] },
          { wait: 2200 },
        ],
      },
      {
        title: "new-laptop — ~",
        caption: `On a new machine: pull the lock from GitHub, then sync it into ${h.root}.`,
        steps: [
          { cmd: "hlock pull github:you/my-app" },
          { out: [ok("✓") + " pulled " + hi("github:you/my-app") + " @ 4e1c9a2 → you"] },
          { out: box(hi("you · pinned"), [...setup, kv("pulled", "github:you/my-app @ 4e1c9a2")]), gap: 45 },
          { out: [warn("!") + " starts MCP servers with: npx  " + mut("· review before syncing into your own config")] },
          { wait: 900 },
          { cmd: "export HLOCK_SECRET_GITHUB_GITHUB_TOKEN=ghp_…" },
          { cmd: "hlock sync you --in-place --yes" },
          { out: [hi("Sync plan") + ` for ${h.name} ${h.version} → your config ${h.root}`] },
          {
            out: [
              ok("✓") + "  " + plan("harness", `${h.name} ${h.version}`),
              ok("✓") + "  " + plan("mcp", "`npx` for MCP servers: github"),
              ok("✓") + "  " + plan("secret", "HLOCK_SECRET_GITHUB_GITHUB_TOKEN (mcp github GITHUB_TOKEN)"),
              ...h.writes.map((w) => auto("●") + "  " + plan("config", "Write " + w)),
              ...copies.map((c) => auto("●") + "  " + plan("skill", c)),
            ],
            gap: 120,
          },
          { out: done.map((d) => ok("✓") + " " + d), gap: 160 },
          { out: [ok("✓") + " in sync with ~/.hlock/pulls/you/harness.lock"] },
          { wait: 3600 },
        ],
      },
    ];
  }
  const scenes = harnesses.flatMap(scenesFor);

  const body = term.querySelector(".hl-term__body");
  const title = term.querySelector(".hl-term__title");
  const caption = term.querySelector(".hl-term__caption");
  // Widest line across all scenes, in characters; the terminal scales its font to fit it.
  const cols = Math.max(...scenes.flatMap((sc) => sc.steps.flatMap((s) =>
    s.cmd ? [s.cmd.length + 2] : (s.out || []).map(plainLen))));
  body.style.setProperty("--cols", cols + 1);

  const line = (html) => {
    body.querySelectorAll(".cursor").forEach((c) => c.remove());
    body.insertAdjacentHTML("beforeend", (body.innerHTML ? "\n" : "") + html);
    body.scrollTop = body.scrollHeight;
  };

  async function type(text) {
    line(span("t-pr", "$ ") + '<span class="t-cmd"></span><span class="cursor"></span>');
    const cmds = body.querySelectorAll(".t-cmd");
    const el = cmds[cmds.length - 1];
    await sleep(350);
    for (const ch of text) {
      el.textContent += ch;
      await sleep(18 + Math.random() * 38);
    }
    await sleep(380);
    body.querySelectorAll(".cursor").forEach((c) => c.remove());
  }

  // Slide the window out to the left and a fresh one in from the right, like switching to
  // another machine's desktop; `swap` runs while it's off screen.
  async function swipe(swap) {
    term.classList.add("is-leaving");
    await sleep(450);
    swap();
    term.classList.remove("is-leaving");
    term.classList.add("is-entering");
    void term.offsetWidth;
    term.classList.remove("is-entering");
    await sleep(450);
  }

  async function play(scene) {
    for (const step of scene.steps) {
      if (step.cmd) await type(step.cmd);
      else if (step.out) {
        for (const l of step.out) {
          line(l);
          await sleep(step.gap ?? 90);
        }
        await sleep(180);
      } else if (step.wait) await sleep(step.wait);
    }
  }

  const show = (scene) => {
    const n = scenes.indexOf(scene) % 2 + 1;
    title.textContent = scene.title;
    caption.innerHTML = `<span class="hl-term__step">${n}/2</span>` + escape(scene.caption);
    body.innerHTML = "";
  };

  async function loop() {
    show(scenes[0]);
    for (let i = 0; ; i = (i + 1) % scenes.length) {
      await play(scenes[i]);
      const next = scenes[(i + 1) % scenes.length];
      await swipe(() => show(next));
    }
  }

  if (reduced) {
    // No animation: print the first harness's two scenes in one window, each under its
    // machine's name.
    term.classList.add("is-static");
    title.textContent = scenes[0].title;
    caption.remove();
    body.innerHTML = scenes.slice(0, 2).flatMap((sc, i) => [
      ...(i ? ["", dim("# " + sc.title)] : []),
      dim("# " + sc.caption),
      ...sc.steps.flatMap((s) => s.cmd ? [span("t-pr", "$ ") + span("t-cmd", escape(s.cmd))] : s.out || []),
    ]).join("\n");
  } else {
    loop();
  }
})();
