//! Harness and runtime profiles: declarative descriptions of agent harnesses
//! (Claude Code, Codex, Pi, OpenCode, …) and local inference runtimes (Ollama,
//! LM Studio, vLLM, …).
//!
//! A profile says where a harness keeps its config, how to find its model, MCP
//! servers, skills and plugins, how it was installed, and how to relocate its
//! config root for an isolated session. Built-in profiles ship inside the crate
//! (`profiles/`); users add or override them with TOML files in a profile
//! directory (see [`profile_dirs`]), so new harnesses need no code change.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

// --------------------------------------------------------------------------- //
// Built-in profiles
// --------------------------------------------------------------------------- //

const BUILTIN_HARNESSES: &[(&str, &str)] = &[
    ("claude-code", include_str!("../profiles/harnesses/claude-code.toml")),
    ("codex", include_str!("../profiles/harnesses/codex.toml")),
    ("pi", include_str!("../profiles/harnesses/pi.toml")),
    ("opencode", include_str!("../profiles/harnesses/opencode.toml")),
    ("hermes", include_str!("../profiles/harnesses/hermes.toml")),
    ("cursor-agent", include_str!("../profiles/harnesses/cursor-agent.toml")),
    ("gemini-cli", include_str!("../profiles/harnesses/gemini-cli.toml")),
    ("copilot-cli", include_str!("../profiles/harnesses/copilot-cli.toml")),
    ("goose", include_str!("../profiles/harnesses/goose.toml")),
    ("amp", include_str!("../profiles/harnesses/amp.toml")),
    // autonomous agents: long-running gateways (`autonomous = true`)
    ("openclaw", include_str!("../profiles/harnesses/openclaw.toml")),
    ("nanobot", include_str!("../profiles/harnesses/nanobot.toml")),
    ("picoclaw", include_str!("../profiles/harnesses/picoclaw.toml")),
    ("zeroclaw", include_str!("../profiles/harnesses/zeroclaw.toml")),
    ("ironclaw", include_str!("../profiles/harnesses/ironclaw.toml")),
    // frameworks: code harnesses a project builds on (`[harnesses.<id>] framework = …`)
    ("pydantic-ai", include_str!("../profiles/harnesses/pydantic-ai.toml")),
    ("deepagents", include_str!("../profiles/harnesses/deepagents.toml")),
    ("langgraph", include_str!("../profiles/harnesses/langgraph.toml")),
    ("openai-agents", include_str!("../profiles/harnesses/openai-agents.toml")),
    ("claude-agent-sdk", include_str!("../profiles/harnesses/claude-agent-sdk.toml")),
    ("google-adk", include_str!("../profiles/harnesses/google-adk.toml")),
    ("smolagents", include_str!("../profiles/harnesses/smolagents.toml")),
    ("crewai", include_str!("../profiles/harnesses/crewai.toml")),
];

const BUILTIN_RUNTIMES: &[(&str, &str)] = &[
    ("ollama", include_str!("../profiles/runtimes/ollama.toml")),
    ("lmstudio", include_str!("../profiles/runtimes/lmstudio.toml")),
    ("llama-cpp", include_str!("../profiles/runtimes/llama-cpp.toml")),
    ("vllm", include_str!("../profiles/runtimes/vllm.toml")),
    ("sglang", include_str!("../profiles/runtimes/sglang.toml")),
    ("mlx-lm", include_str!("../profiles/runtimes/mlx-lm.toml")),
    ("localai", include_str!("../profiles/runtimes/localai.toml")),
    ("docker-model-runner", include_str!("../profiles/runtimes/docker-model-runner.toml")),
    ("jan", include_str!("../profiles/runtimes/jan.toml")),
    ("ollaya", include_str!("../profiles/runtimes/ollaya.toml")),
];

// --------------------------------------------------------------------------- //
// Environment context
// --------------------------------------------------------------------------- //

/// The slice of the process environment that profiles read: home and working
/// directories, environment variables, and the `PATH` search list. Tests build
/// one by hand so nothing depends on the real machine.
#[derive(Debug, Clone)]
pub struct Ctx {
    pub home: PathBuf,
    pub cwd: PathBuf,
    pub env: BTreeMap<String, String>,
    pub path: Vec<PathBuf>,
}

impl Ctx {
    pub fn from_process() -> Ctx {
        let mut env: BTreeMap<String, String> = std::env::vars().collect();
        crate::meta::restore_env(&mut env);
        let home = env.get("HOME").or_else(|| env.get("USERPROFILE")).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
        let path = env.get("PATH").map(|p| std::env::split_paths(p).collect()).unwrap_or_default();
        Ctx {
            home,
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            env,
            path,
        }
    }

    /// The process context with `cwd` set to a project directory (where the manifest
    /// lives), so project-relative paths resolve there and not in a subdirectory.
    pub fn at(root: &Path) -> Ctx {
        let mut c = Ctx::from_process();
        c.cwd = if root.is_absolute() { root.to_path_buf() } else { c.cwd.join(root) };
        c
    }

    /// A non-empty environment variable.
    pub fn var(&self, key: &str) -> Option<&str> {
        self.env.get(key).map(String::as_str).filter(|v| !v.is_empty())
    }

    /// Expand `~`, `{root}`, `{home}` and relative (project) paths.
    pub fn expand(&self, template: &str, root: Option<&Path>) -> PathBuf {
        let mut s = template.to_string();
        if let Some(r) = root {
            s = s.replace("{root}", &r.to_string_lossy());
        }
        s = s.replace("{home}", &self.home.to_string_lossy());
        if s == "~" {
            return self.home.clone();
        }
        if let Some(rest) = s.strip_prefix("~/") {
            return self.home.join(rest);
        }
        let p = PathBuf::from(&s);
        if p.is_absolute() {
            p
        } else {
            self.cwd.join(p)
        }
    }

    /// Resolve a binary name on `PATH`.
    pub fn which(&self, name: &str) -> Option<PathBuf> {
        if name.contains('/') {
            let p = PathBuf::from(name);
            return p.is_file().then_some(p);
        }
        for dir in &self.path {
            let cand = dir.join(name);
            if cand.is_file() {
                return Some(cand);
            }
        }
        None
    }
}

// --------------------------------------------------------------------------- //
// Profile schema
// --------------------------------------------------------------------------- //

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "kebab-case")]
pub struct VersionProbe {
    pub args: Vec<String>,
}

/// Where the harness keeps its user-level config, and the variable that moves it.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "kebab-case")]
pub struct ConfigRoot {
    /// Variable that relocates the root (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`, …).
    pub env: Option<String>,
    /// Appended to `env` when it names a parent dir (`XDG_CONFIG_HOME` + `opencode`).
    pub suffix: Option<String>,
    pub default: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "kebab-case")]
pub struct InstallMethod {
    /// `npm` | `brew` | `brew-cask` | `pip` | `uv-tool` | `script` | `manual`
    pub method: String,
    pub package: Option<String>,
    /// Shell command for `script` / instructions for `manual` (`{version}` is substituted).
    pub command: Option<String>,
    /// Substrings of the binary's real path that identify this install method.
    pub path_hints: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "kebab-case")]
pub struct FileSpec {
    pub path: String,
    /// Location when the config root is relocated (for files that live outside it).
    pub isolated: Option<String>,
    /// `json` | `jsonc` | `json5` | `toml` | `yaml`
    pub format: String,
    /// `whole` (restore the file) | `pointers` (restore only `pointers`) | `record` (digest only)
    pub restore: String,
    pub pointers: Vec<String>,
    /// How the harness reads an environment variable inside this file, with `{name}`
    /// for the variable: `${{name}}` (Claude Code), `{env:{name}}` (OpenCode). Set
    /// only when the harness expands it wherever hlock writes secrets in the file;
    /// `hlock launch` then writes the reference and passes the value in the environment.
    pub env_ref: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "kebab-case")]
pub struct ModelSpec {
    /// Ordered sources (`env:VAR` or `<file>#<json-pointer>`); the first hit wins.
    pub default: Vec<String>,
    pub small: Vec<String>,
    pub provider: Vec<String>,
    pub base_url: Vec<String>,
    /// `provider/model` when the model value carries its provider.
    pub format: Option<String>,
    pub default_provider: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "kebab-case")]
pub struct ProviderSpec {
    pub source: Vec<String>,
    /// Pointers (relative to one provider entry) to its base URL.
    pub base_url: Vec<String>,
    pub models: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "kebab-case")]
pub struct McpSpec {
    pub sources: Vec<String>,
    pub write: Option<String>,
    /// How servers are written: `standard` (`command`/`args`/`env`, `url`/`headers`),
    /// `opencode`, `goose`, or `codex`.
    pub shape: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "kebab-case")]
pub struct SkillsSpec {
    pub dirs: Vec<String>,
    pub write: Option<String>,
    /// Directories whose skills ship with the harness itself.
    pub builtin_dirs: Vec<String>,
    /// A file listing the harness's bundled skills, one per line or as keys.
    pub builtin_manifest: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "kebab-case")]
pub struct BundleSpec {
    /// `plugin` | `package` | `extension`
    pub kind: String,
    /// `claude-plugins` | `pi-packages` | `npm-list`
    pub extractor: String,
    pub source: String,
    pub lockfile: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "kebab-case")]
pub struct LaunchSpec {
    pub args: Vec<String>,
    pub model_flag: Option<String>,
    /// Arguments that run one prompt non-interactively and exit (`hlock run`, and
    /// `hlock meta run --headless`), with `{prompt}` replaced by the prompt, e.g.
    /// `["-p", "{prompt}"]`.
    pub prompt: Vec<String>,
    /// Arguments that start an interactive session whose first message is `{prompt}`
    /// (`hlock meta run`).
    pub interactive: Vec<String>,
}

/// How `hlock localize` points a harness at a local OpenAI-compatible endpoint:
/// values to set (by JSON pointer) in one of the profile's files. Strings and keys
/// may use `{provider}`, `{model}`, `{base_url}` and `{host}`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "kebab-case")]
pub struct LocalProvider {
    pub file: String,
    pub set: BTreeMap<String, serde_json::Value>,
    /// Apply only when localizing onto one of these runtimes (empty: any). Entries
    /// apply in order, so a runtime-specific entry after a generic one overrides it.
    pub runtimes: Vec<String>,
}

impl LocalProvider {
    /// Whether this entry applies on `runtime` (`None`: an API provider, not a runtime).
    pub fn applies_to(&self, runtime: Option<&str>) -> bool {
        self.runtimes.is_empty() || runtime.is_some_and(|rt| self.runtimes.iter().any(|r| r == rt))
    }
}

/// An API provider a harness supports, and the environment variable holding its key.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "kebab-case")]
pub struct ApiKey {
    pub provider: String,
    pub env: String,
}

/// A model `hlock localize` may pick for a runtime.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "kebab-case")]
pub struct CatalogModel {
    pub id: String,
    pub params_b: Option<f64>,
    /// Memory the weights plus a working context need.
    pub min_memory_gb: Option<f64>,
    /// 3 strong agentic coder, 2 capable, 1 small/fast.
    pub tier: u8,
    /// Reliable tool calling.
    pub tools: bool,
    /// Roles it suits (empty: any).
    pub roles: Vec<String>,
}

/// How hlock runs a code harness (a framework agent or a custom script).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "kebab-case")]
pub struct RunSpec {
    /// Python `module:callable`, called as `fn(prompt, ctx)`.
    pub entrypoint: Option<String>,
    /// Any executable; `{prompt}`, `{model}` and `{workspace}` are substituted.
    pub command: Vec<String>,
    /// Interpreter for `entrypoint` (default: `HL_PYTHON`, else `python3`).
    pub python: Option<String>,
    pub env: BTreeMap<String, String>,
}

/// How a harness reaches a decider for one use (`[deciders.<use>]` in a profile).
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default, rename_all = "kebab-case")]
pub struct DeciderWiringSpec {
    /// A hook hlock writes: into the profile file `file`, under `pointer` (default
    /// `/hooks`), for `events`, with output in `format` (`claude-code`).
    pub hook: Option<HookSpec>,
    /// A setting of the harness's own that names the model (`config#/auxiliary/approval`),
    /// with the keys it uses for provider, model and base URL.
    pub native: Option<NativeDeciderSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default, rename_all = "kebab-case")]
pub struct HookSpec {
    /// A profile file the hooks are merged into (`settings`)…
    pub file: String,
    /// …or a path of its own (`{root}/hooks.json`, `.github/hooks/hlock.json`): merged
    /// into for `cursor`, written whole for `copilot` and the plugin formats (`amp`,
    /// `opencode`, `pi`), which hlock owns.
    pub path: Option<String>,
    pub pointer: Option<String>,
    pub events: Vec<String>,
    /// `claude-code`, `codex`, `gemini`, `cursor`, `copilot`, `amp`, `opencode`, `pi`
    pub format: String,
}

impl HookSpec {
    /// What a lock's `[[decider.wiring]]` names: the profile file, or the path.
    pub fn target(&self) -> String {
        self.path.clone().unwrap_or_else(|| self.file.clone())
    }

    /// Whether hlock writes the whole file (a plugin, Copilot's hook file).
    pub fn owned(&self) -> bool {
        self.path.is_some() && matches!(self.format.as_str(), "copilot" | "amp" | "opencode" | "pi")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default, rename_all = "kebab-case")]
pub struct NativeDeciderSpec {
    pub file: String,
    pub pointer: String,
    pub provider_key: Option<String>,
    pub model_key: String,
    pub base_url_key: Option<String>,
}

/// A declarative description of one agent harness.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "kebab-case")]
pub struct HarnessProfile {
    pub id: String,
    pub name: String,
    /// `cli` (an installed agent CLI), `framework` (a Python/JS agent library), or
    /// `custom` (a project's own agent, declared under `[harnesses.<id>]`).
    pub kind: String,
    /// A long-running, self-directed agent (a gateway with chat channels, schedules
    /// and memory) rather than a session a person drives. Docs and `hlock harness list`
    /// group these apart; everything else treats them like any other `cli` harness.
    pub autonomous: bool,
    pub aliases: Vec<String>,
    pub vendor: Option<String>,
    pub homepage: Option<String>,
    /// `full` (pin, sync and launch are tested) or `basic` (detect, version, MCP, skills).
    pub support: String,
    /// Open-source license (for `hlock localize`, which prefers open harnesses).
    pub open_source: Option<bool>,
    /// Can use a local OpenAI- or Anthropic-compatible model endpoint.
    pub local_models: Option<bool>,
    /// How to configure a local model endpoint (`hlock localize`).
    pub local_provider: Vec<LocalProvider>,
    /// API providers the harness talks to natively, and the variable each reads its key from.
    pub api_keys: Vec<ApiKey>,
    /// How to set an API provider and model (`hlock meta`): like `local-provider`, with
    /// `{provider}` and `{model}` substituted.
    pub api_provider: Vec<LocalProvider>,
    /// Framework profile a custom harness builds on (`pydantic-ai`, `deepagents`, …).
    pub framework: Option<String>,
    /// Packages whose installed versions are pinned (`pypi:pydantic-ai`, `npm:…`).
    pub packages: Vec<String>,
    /// How to run a code harness.
    pub run: RunSpec,
    pub binaries: Vec<String>,
    pub version: VersionProbe,
    /// Variables a harness sets in the processes it spawns (detects "running inside").
    pub active_env: Vec<String>,
    pub config_root: ConfigRoot,
    pub instructions: Vec<String>,
    pub install: Vec<InstallMethod>,
    pub files: BTreeMap<String, FileSpec>,
    pub model: ModelSpec,
    pub providers: Option<ProviderSpec>,
    pub mcp: McpSpec,
    pub skills: SkillsSpec,
    pub bundles: Vec<BundleSpec>,
    pub launch: LaunchSpec,
    /// How each decider use is wired into this harness (`approve`, `stop`, …).
    pub deciders: BTreeMap<String, DeciderWiringSpec>,
    /// Environment variables a framework reads a System One endpoint from
    /// (`SYSTEM_ONE_BASE_URL`, `TYPESAFE_BASE_URL`): base-URL and key variable pairs.
    pub decider_env: Vec<(String, String)>,
    /// Where the profile came from: `builtin` or a file path.
    #[serde(skip_deserializing)]
    pub origin: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "kebab-case")]
pub struct RuntimeModels {
    /// `ollama` | `publisher-repo` | `hf-cache`
    pub layout: String,
    /// Ordered model directories (`env:VAR` entries are honored first).
    pub dirs: Vec<String>,
}

/// A declarative description of one local inference runtime.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "kebab-case")]
pub struct RuntimeProfile {
    pub id: String,
    pub name: String,
    pub aliases: Vec<String>,
    pub homepage: Option<String>,
    pub binaries: Vec<String>,
    pub version: VersionProbe,
    /// Default ports, used to recognize a provider's base URL.
    pub ports: Vec<u16>,
    /// Provider names harness configs use for this runtime.
    pub provider_names: Vec<String>,
    /// Command template to fetch a model (`{model}`, `{repo}`, `{revision}`).
    pub pull: Option<String>,
    pub serve: Option<String>,
    pub models: RuntimeModels,
    pub install: Vec<InstallMethod>,
    /// Models `hlock localize` may pick.
    pub catalog: Vec<CatalogModel>,
    #[serde(skip_deserializing)]
    pub origin: String,
}

impl HarnessProfile {
    pub fn matches(&self, name: &str) -> bool {
        let n = name.to_ascii_lowercase();
        self.id == n || self.aliases.iter().any(|a| a.to_ascii_lowercase() == n)
    }

    /// The live config root: the relocation variable if set, else the default.
    pub fn config_root(&self, ctx: &Ctx) -> PathBuf {
        let cr = &self.config_root;
        if let Some(var) = &cr.env {
            if let Some(v) = ctx.var(var) {
                let base = ctx.expand(v, None);
                return match &cr.suffix {
                    Some(sfx) => base.join(sfx),
                    None => base,
                };
            }
        }
        ctx.expand(&cr.default, None)
    }

    /// Whether `config_root` is currently relocated by its variable.
    pub fn root_relocated(&self, ctx: &Ctx) -> bool {
        self.config_root.env.as_deref().and_then(|v| ctx.var(v)).is_some()
    }

    /// Resolve a named file to a path for the live environment.
    pub fn file_path(&self, id: &str, ctx: &Ctx) -> Option<PathBuf> {
        let f = self.files.get(id)?;
        let root = self.config_root(ctx);
        let tpl = match (&f.isolated, self.root_relocated(ctx)) {
            (Some(iso), true) => iso,
            _ => &f.path,
        };
        Some(ctx.expand(tpl, Some(&root)))
    }

    /// Environment that relocates this harness's config root to `dir`, used by
    /// isolated sessions. `None` when the harness has no relocation variable.
    pub fn isolation_env(&self, dir: &Path) -> Option<(String, PathBuf)> {
        let var = self.config_root.env.clone()?;
        let value = match &self.config_root.suffix {
            // XDG-style: the variable names the parent of the root.
            Some(_) => dir.parent().map(Path::to_path_buf).unwrap_or_else(|| dir.to_path_buf()),
            None => dir.to_path_buf(),
        };
        Some((var, value))
    }

    /// True when the current process runs inside this harness.
    pub fn is_active(&self, ctx: &Ctx) -> bool {
        self.active_env.iter().any(|v| ctx.var(v).is_some())
    }
}

impl RuntimeProfile {
    pub fn matches(&self, name: &str) -> bool {
        let n = name.to_ascii_lowercase();
        self.id == n || self.aliases.iter().any(|a| a.to_ascii_lowercase() == n)
    }

    /// The first model directory that exists (falls back to the first candidate).
    pub fn models_dir(&self, ctx: &Ctx) -> Option<PathBuf> {
        let mut first = None;
        for d in &self.models.dirs {
            let p = match d.strip_prefix("env:") {
                Some(var) => match ctx.var(var) {
                    Some(v) => ctx.expand(v, None),
                    None => continue,
                },
                None => ctx.expand(d, None),
            };
            if p.is_dir() {
                return Some(p);
            }
            first.get_or_insert(p);
        }
        first
    }
}

// --------------------------------------------------------------------------- //
// Registry
// --------------------------------------------------------------------------- //

/// All known profiles: the built-ins, overridden or extended by user profile dirs.
#[derive(Debug, Clone, Default)]
pub struct Registry {
    pub harnesses: Vec<HarnessProfile>,
    pub runtimes: Vec<RuntimeProfile>,
    /// Project profiles (or parts of them) that were skipped, and why.
    pub notes: Vec<String>,
}

/// Profile directories, lowest precedence first: `~/.config/hlock/profiles`,
/// each entry of `HLOCK_PROFILE_PATH`, then `./.hlock/profiles`. Each holds
/// `harnesses/*.toml` and `runtimes/*.toml`; a later profile with the same `id`
/// replaces an earlier one, except that the project's own directory (the last; see
/// [`project_profile_dir`]) may only add profiles.
pub fn profile_dirs(ctx: &Ctx) -> Vec<PathBuf> {
    let mut dirs = vec![ctx.expand("~/.config/hlock/profiles", None)];
    if let Some(p) = ctx.var("HLOCK_PROFILE_PATH") {
        dirs.extend(std::env::split_paths(p));
    }
    dirs.push(project_profile_dir(ctx));
    dirs
}

/// The project's own profile directory, `./.hlock/profiles`. Whoever wrote the
/// repository controls it, so [`Registry::extend_from_project_dir`] limits what it can do.
pub fn project_profile_dir(ctx: &Ctx) -> PathBuf {
    ctx.cwd.join(".hlock/profiles")
}

/// Version arguments a project profile may probe with.
const VERSION_FLAGS: &[&str] = &["--version", "-version", "-V", "-v", "version"];

/// Keep a project profile from running anything but a program on `PATH` with a
/// version flag: `hlock check` probes every profile's binaries.
fn confine(notes: &mut Vec<String>, origin: &Path, binaries: &mut Vec<String>, version: &mut VersionProbe, python: Option<&mut Option<String>>) {
    let by_path = |b: &str| b.contains(['/', '\\']);
    for b in binaries.iter().filter(|b| by_path(b)) {
        notes.push(format!("{}: ignored binary {b:?}; a project profile names programs on PATH, not by path", origin.display()));
    }
    binaries.retain(|b| !by_path(b));
    if !version.args.iter().all(|a| VERSION_FLAGS.contains(&a.as_str())) {
        notes.push(format!("{}: ignored version args {:?}; a project profile may only probe with a version flag", origin.display(), version.args));
        version.args.clear();
    }
    if let Some(py) = python {
        if py.as_deref().is_some_and(by_path) {
            notes.push(format!("{}: ignored python {:?}; a project profile names an interpreter on PATH, not by path", origin.display(), py.as_deref().unwrap_or_default()));
            *py = None;
        }
    }
}

pub fn parse_harness(text: &str, origin: &str) -> Result<HarnessProfile> {
    let mut p: HarnessProfile =
        toml::from_str(text).map_err(|e| Error::Profile(format!("invalid harness profile {origin}: {e}")))?;
    if p.id.is_empty() {
        return Err(Error::Profile(format!("harness profile {origin} has no id")));
    }
    if p.name.is_empty() {
        p.name = p.id.clone();
    }
    if p.support.is_empty() {
        p.support = "basic".into();
    }
    if p.kind.is_empty() {
        p.kind = if p.run.entrypoint.is_some() || !p.run.command.is_empty() { "custom" } else { "cli" }.into();
    }
    p.origin = origin.to_string();
    Ok(p)
}

pub fn parse_runtime(text: &str, origin: &str) -> Result<RuntimeProfile> {
    let mut p: RuntimeProfile =
        toml::from_str(text).map_err(|e| Error::Profile(format!("invalid runtime profile {origin}: {e}")))?;
    if p.id.is_empty() {
        return Err(Error::Profile(format!("runtime profile {origin} has no id")));
    }
    if p.name.is_empty() {
        p.name = p.id.clone();
    }
    p.origin = origin.to_string();
    Ok(p)
}

impl Registry {
    /// Only the profiles compiled into this build.
    pub fn builtin() -> Registry {
        let harnesses = BUILTIN_HARNESSES
            .iter()
            .map(|(id, text)| parse_harness(text, "builtin").unwrap_or_else(|e| panic!("builtin profile {id}: {e}")))
            .collect();
        let runtimes = BUILTIN_RUNTIMES
            .iter()
            .map(|(id, text)| parse_runtime(text, "builtin").unwrap_or_else(|e| panic!("builtin profile {id}: {e}")))
            .collect();
        Registry { harnesses, runtimes, notes: Vec::new() }
    }

    /// Built-ins plus every profile found in [`profile_dirs`].
    pub fn load(ctx: &Ctx) -> Result<Registry> {
        let mut reg = Registry::builtin();
        let project = project_profile_dir(ctx);
        for dir in profile_dirs(ctx) {
            if dir == project {
                reg.extend_from_project_dir(&dir)?;
            } else {
                reg.extend_from_dir(&dir)?;
            }
        }
        Ok(reg)
    }

    /// Add the profiles in a project's own profile directory. They may add new ids but
    /// not replace a profile hlock already has (put an override in
    /// `~/.config/hlock/profiles`), and they can't name a program by path or probe it
    /// with anything but a version flag. What's skipped is noted in `notes`.
    pub fn extend_from_project_dir(&mut self, dir: &Path) -> Result<()> {
        let taken = |notes: &mut Vec<String>, path: &Path, kind: &str, id: &str| {
            notes.push(format!("{}: skipped; a project profile can't replace the {kind} profile {id:?} (override it in ~/.config/hlock/profiles)", path.display()));
        };
        for path in toml_files(&dir.join("harnesses")) {
            let text = std::fs::read_to_string(&path)?;
            let mut p = parse_harness(&text, &path.to_string_lossy())?;
            if self.harnesses.iter().any(|h| h.id == p.id) {
                taken(&mut self.notes, &path, "harness", &p.id);
                continue;
            }
            confine(&mut self.notes, &path, &mut p.binaries, &mut p.version, Some(&mut p.run.python));
            self.harnesses.push(p);
        }
        for path in toml_files(&dir.join("runtimes")) {
            let text = std::fs::read_to_string(&path)?;
            let mut p = parse_runtime(&text, &path.to_string_lossy())?;
            if self.runtimes.iter().any(|r| r.id == p.id) {
                taken(&mut self.notes, &path, "runtime", &p.id);
                continue;
            }
            confine(&mut self.notes, &path, &mut p.binaries, &mut p.version, None);
            self.runtimes.push(p);
        }
        Ok(())
    }

    /// Add (or replace by id) the profiles in `dir/harnesses` and `dir/runtimes`.
    pub fn extend_from_dir(&mut self, dir: &Path) -> Result<()> {
        for path in toml_files(&dir.join("harnesses")) {
            let text = std::fs::read_to_string(&path)?;
            let p = parse_harness(&text, &path.to_string_lossy())?;
            self.harnesses.retain(|h| h.id != p.id);
            self.harnesses.push(p);
        }
        for path in toml_files(&dir.join("runtimes")) {
            let text = std::fs::read_to_string(&path)?;
            let p = parse_runtime(&text, &path.to_string_lossy())?;
            self.runtimes.retain(|r| r.id != p.id);
            self.runtimes.push(p);
        }
        Ok(())
    }

    /// Built-ins, profile dirs, then the manifest's own `[harnesses.<id>]` tables.
    pub fn load_for(ctx: &Ctx, manifest: Option<&crate::manifest::Manifest>) -> Result<Registry> {
        let mut reg = Registry::load(ctx)?;
        if let Some(m) = manifest {
            reg.add_project_harnesses(m)?;
        }
        Ok(reg)
    }

    /// Register a manifest's `[harnesses.<id>]` tables as `custom` profiles. A
    /// table with `framework = "<id>"` inherits that framework's packages.
    pub fn add_project_harnesses(&mut self, m: &crate::manifest::Manifest) -> Result<()> {
        let origin = m
            .source_path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| crate::MANIFEST_PATH.to_string());
        for (id, table) in &m.harnesses {
            let mut v = table.clone();
            if let Some(obj) = v.as_object_mut() {
                obj.entry("id").or_insert_with(|| serde_json::json!(id));
                // `entrypoint`, `command`, `python` and `env` may sit at the top level.
                let mut run = obj.remove("run").and_then(|r| r.as_object().cloned()).unwrap_or_default();
                for key in ["entrypoint", "command", "python", "env"] {
                    if let Some(val) = obj.remove(key) {
                        run.insert(key.to_string(), val);
                    }
                }
                obj.insert("run".into(), serde_json::Value::Object(run));
            }
            let mut p: HarnessProfile = serde_json::from_value(v)
                .map_err(|e| Error::Profile(format!("invalid [harnesses.{id}] in {origin}: {e}")))?;
            if p.name.is_empty() {
                p.name = id.clone();
            }
            if p.kind.is_empty() {
                p.kind = "custom".into();
            }
            if p.support.is_empty() {
                p.support = "basic".into();
            }
            if let Some(fw) = &p.framework {
                let base = self
                    .harness(fw)
                    .filter(|h| h.kind == "framework")
                    .ok_or_else(|| Error::Profile(format!("[harnesses.{id}] framework {fw:?} is not a known framework")))?;
                for pkg in &base.packages {
                    if !p.packages.contains(pkg) {
                        p.packages.push(pkg.clone());
                    }
                }
                if p.homepage.is_none() {
                    p.homepage = base.homepage.clone();
                }
                if p.decider_env.is_empty() {
                    p.decider_env = base.decider_env.clone();
                }
            }
            if p.kind == "custom" && p.run.entrypoint.is_none() && p.run.command.is_empty() {
                return Err(Error::Profile(format!(
                    "[harnesses.{id}] needs an `entrypoint` (python module:callable) or a `command`"
                )));
            }
            p.origin = origin.clone();
            self.harnesses.retain(|h| h.id != p.id);
            self.harnesses.push(p);
        }
        Ok(())
    }

    pub fn harness(&self, name: &str) -> Option<&HarnessProfile> {
        self.harnesses.iter().find(|h| h.matches(name))
    }

    pub fn runtime(&self, name: &str) -> Option<&RuntimeProfile> {
        self.runtimes.iter().find(|r| r.matches(name))
    }

    /// Look a harness up by id or alias, with a helpful error listing what exists.
    pub fn require_harness(&self, name: &str) -> Result<&HarnessProfile> {
        self.harness(name).ok_or_else(|| {
            let known: Vec<&str> = self.harnesses.iter().map(|h| h.id.as_str()).collect();
            Error::Profile(format!("unknown harness {name:?} (known: {})", known.join(", ")))
        })
    }

    /// The harness this process is running inside, if any.
    pub fn active(&self, ctx: &Ctx) -> Option<&HarnessProfile> {
        self.harnesses.iter().find(|h| h.is_active(ctx))
    }

    /// Pick the harness a command should target, in precedence order: an explicit
    /// name, `HLOCK_HARNESS`, the manifest's `[target] harness`, the harness this
    /// process runs inside, then the only installed harness.
    pub fn select(&self, explicit: Option<&str>, manifest_target: Option<&str>, ctx: &Ctx) -> Result<Selection> {
        let named = explicit
            .map(|n| (n.to_string(), "--harness"))
            .or_else(|| ctx.var("HLOCK_HARNESS").map(|n| (n.to_string(), "HLOCK_HARNESS")))
            .or_else(|| manifest_target.map(|n| (n.to_string(), "harness.toml [target]")));
        if let Some((name, why)) = named {
            let p = self.require_harness(&name)?;
            return Ok(Selection { id: p.id.clone(), reason: why.to_string() });
        }
        if let Some(p) = self.active(ctx) {
            return Ok(Selection { id: p.id.clone(), reason: "running inside it".into() });
        }
        let installed: Vec<&HarnessProfile> = self
            .harnesses
            .iter()
            .filter(|h| h.kind == "cli" && h.binaries.iter().any(|b| ctx.which(b).is_some()))
            .collect();
        match installed.as_slice() {
            [one] => Ok(Selection { id: one.id.clone(), reason: "the only installed harness".into() }),
            [] => Err(Error::Profile("no supported harness found on PATH".into())),
            many => Err(Error::Profile(format!(
                "several harnesses are installed ({}); pick one with --harness",
                many.iter().map(|h| h.id.as_str()).collect::<Vec<_>>().join(", ")
            ))),
        }
    }
}

/// The outcome of [`Registry::select`]: which harness, and why it was chosen.
#[derive(Debug, Clone, Serialize)]
pub struct Selection {
    pub id: String,
    pub reason: String,
}

fn toml_files(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == "toml"))
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

// --------------------------------------------------------------------------- //
// Probing an installed harness / runtime
// --------------------------------------------------------------------------- //

/// What's installed on this machine for one profile.
#[derive(Debug, Clone, Serialize, Default)]
pub struct Probe {
    pub id: String,
    pub installed: bool,
    pub binary: Option<String>,
    pub version: Option<String>,
    /// Detected install method (`npm`, `brew-cask`, `script`, …).
    pub method: Option<String>,
    /// Reproducible source spec, e.g. `npm:@openai/codex@0.51.0`.
    pub source: Option<String>,
    pub config_root: Option<String>,
    pub active: bool,
    /// Installed versions of the profile's `packages` (code harnesses).
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub packages: BTreeMap<String, String>,
}

const VERSION_TIMEOUT: Duration = Duration::from_secs(8);

pub fn probe_harness(p: &HarnessProfile, ctx: &Ctx) -> Probe {
    if p.kind != "cli" {
        return probe_code_harness(p, ctx);
    }
    let mut out = probe_binary(&p.id, &p.binaries, &p.version, &p.install, ctx);
    out.config_root = Some(p.config_root(ctx).to_string_lossy().into_owned());
    out.active = p.is_active(ctx);
    out
}

/// Frameworks and custom harnesses: "installed" means their packages import
/// (or a custom command resolves), and the version is the first package's.
fn probe_code_harness(p: &HarnessProfile, ctx: &Ctx) -> Probe {
    let mut out = Probe { id: p.id.clone(), active: p.is_active(ctx), ..Default::default() };
    out.packages = python_package_versions(&p.packages, p.run.python.as_deref(), ctx);
    let first = p.packages.iter().find_map(|pkg| out.packages.get(pkg).map(|v| (pkg, v)));
    if let Some((pkg, v)) = first {
        out.version = Some(v.clone());
        out.method = Some("pip".into());
        out.source = Some(format!("pypi:{}=={v}", pkg.trim_start_matches("pypi:")));
    }
    out.installed = match (p.kind.as_str(), p.run.command.first()) {
        ("custom", Some(cmd)) => {
            out.binary = ctx.which(cmd).or_else(|| Some(ctx.expand(cmd, None)).filter(|x| x.is_file())).map(|b| b.to_string_lossy().into_owned());
            out.binary.is_some()
        }
        ("custom", None) => p.run.entrypoint.is_some(),
        _ => first.is_some(),
    };
    out
}

/// The Python interpreter for code harnesses: an explicit one, `HL_PYTHON`, then `python3`.
pub fn python_for(explicit: Option<&str>, ctx: &Ctx) -> Option<PathBuf> {
    let name = explicit.map(str::to_string).or_else(|| ctx.var("HL_PYTHON").map(str::to_string)).unwrap_or_else(|| "python3".into());
    ctx.which(&name)
}

/// Installed versions of `pypi:` packages, read with one interpreter call.
pub fn python_package_versions(packages: &[String], python: Option<&str>, ctx: &Ctx) -> BTreeMap<String, String> {
    let names: Vec<&str> = packages.iter().filter_map(|p| p.strip_prefix("pypi:")).collect();
    let mut out = BTreeMap::new();
    if names.is_empty() {
        return out;
    }
    let Some(py) = python_for(python, ctx) else { return out };
    let script = format!(
        "import json, importlib.metadata as m\nout = {{}}\nfor n in {names:?}:\n    try: out[n] = m.version(n)\n    except Exception: pass\nprint(json.dumps(out))"
    );
    let Some(text) = run_capture(&py, &["-c".to_string(), script], ctx, VERSION_TIMEOUT) else { return out };
    let Some(line) = text.lines().find(|l| l.trim_start().starts_with('{')) else { return out };
    if let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(line) {
        for (k, v) in map {
            if let Some(v) = v.as_str() {
                out.insert(format!("pypi:{k}"), v.to_string());
            }
        }
    }
    out
}

pub fn probe_runtime(p: &RuntimeProfile, ctx: &Ctx) -> Probe {
    probe_binary(&p.id, &p.binaries, &p.version, &p.install, ctx)
}

fn probe_binary(id: &str, binaries: &[String], version: &VersionProbe, install: &[InstallMethod], ctx: &Ctx) -> Probe {
    let mut out = Probe { id: id.to_string(), ..Default::default() };
    let Some(bin) = binaries.iter().find_map(|b| ctx.which(b)) else {
        return out;
    };
    out.installed = true;
    out.binary = Some(bin.to_string_lossy().into_owned());
    if !version.args.is_empty() {
        out.version = run_capture(&bin, &version.args, ctx, VERSION_TIMEOUT).and_then(|t| extract_version(&t));
    }
    let real = std::fs::canonicalize(&bin).unwrap_or(bin.clone());
    let (method, package) = detect_install(&real, install);
    out.source = method.as_deref().map(|m| source_spec(m, package.as_deref(), out.version.as_deref()));
    out.method = method;
    out
}

/// Format a reproducible source spec for an install method.
pub fn source_spec(method: &str, package: Option<&str>, version: Option<&str>) -> String {
    match (method, package, version) {
        ("pip", Some(p), Some(v)) => format!("pypi:{p}=={v}"),
        ("uv-tool", Some(p), Some(v)) => format!("pypi:{p}=={v}"),
        (m, Some(p), Some(v)) => format!("{m}:{p}@{v}"),
        (m, Some(p), None) => format!("{m}:{p}"),
        (m, None, Some(v)) => format!("{m}@{v}"),
        (m, None, None) => m.to_string(),
    }
}

/// Infer how a binary was installed from its real path.
pub fn detect_install(real: &Path, install: &[InstallMethod]) -> (Option<String>, Option<String>) {
    let s = real.to_string_lossy().replace('\\', "/");
    if s.contains("/node_modules/") {
        let pkg = npm_package_name(real).or_else(|| find_method(install, "npm").and_then(|m| m.package.clone()));
        return (Some("npm".into()), pkg);
    }
    if let Some(rest) = s.split("/Caskroom/").nth(1) {
        return (Some("brew-cask".into()), rest.split('/').next().map(str::to_string));
    }
    if let Some(rest) = s.split("/Cellar/").nth(1) {
        return (Some("brew".into()), rest.split('/').next().map(str::to_string));
    }
    if s.contains("/uv/tools/") {
        let pkg = s.split("/uv/tools/").nth(1).and_then(|r| r.split('/').next()).map(str::to_string);
        return (Some("uv-tool".into()), pkg);
    }
    if s.contains("/site-packages/") || s.contains("/pipx/venvs/") {
        return (Some("pip".into()), find_method(install, "pip").and_then(|m| m.package.clone()));
    }
    for m in install {
        if m.path_hints.iter().any(|h| s.contains(h.as_str())) {
            return (Some(m.method.clone()), m.package.clone());
        }
    }
    (None, None)
}

fn find_method<'a>(install: &'a [InstallMethod], method: &str) -> Option<&'a InstallMethod> {
    install.iter().find(|m| m.method == method)
}

/// The `name` of the nearest `package.json` above a file inside `node_modules`.
fn npm_package_name(file: &Path) -> Option<String> {
    let mut dir = file.parent();
    while let Some(d) = dir {
        let pj = d.join("package.json");
        if pj.is_file() {
            let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&pj).ok()?).ok()?;
            if let Some(name) = v.get("name").and_then(|n| n.as_str()) {
                return Some(name.to_string());
            }
        }
        if d.file_name().is_some_and(|n| n == "node_modules") {
            break;
        }
        dir = d.parent();
    }
    None
}

/// Run a command with a timeout, returning stdout+stderr when it exits 0. `None` when
/// it can't start, times out, or fails.
pub fn run_capture(bin: &Path, args: &[String], ctx: &Ctx, timeout: Duration) -> Option<String> {
    let mut cmd = Command::new(bin);
    cmd.args(args).current_dir(&ctx.cwd).env_clear().envs(&ctx.env).stdin(Stdio::null());
    let out = crate::process::run(cmd, &crate::process::Limits::timeout(timeout)).ok()?;
    if !out.success {
        return None;
    }
    let mut text = out.stdout_text();
    text.push('\n');
    text.push_str(&out.stderr_text());
    Some(text)
}

/// The first dotted version number in `text`, e.g. `2.1.4` or `2026.05.28-a70ca7c`.
/// ANSI escape sequences are skipped.
pub fn extract_version(text: &str) -> Option<String> {
    let clean = strip_ansi(text);
    let b = clean.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let boundary = i == 0
            || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'.')
            || (matches!(b[i - 1], b'v' | b'V') && (i == 1 || !b[i - 2].is_ascii_alphanumeric()));
        if b[i].is_ascii_digit() && boundary {
            let start = i;
            let mut dots = 0;
            while i < b.len() && (b[i].is_ascii_digit() || (b[i] == b'.' && i + 1 < b.len() && b[i + 1].is_ascii_digit())) {
                if b[i] == b'.' {
                    dots += 1;
                }
                i += 1;
            }
            if dots >= 1 {
                // optional pre-release / build suffix
                if i < b.len() && (b[i] == b'-' || b[i] == b'+') && i + 1 < b.len() && b[i + 1].is_ascii_alphanumeric() {
                    i += 1;
                    while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'.') {
                        i += 1;
                    }
                }
                return Some(clean[start..i].trim_end_matches('.').to_string());
            }
        }
        i += 1;
    }
    None
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for n in chars.by_ref() {
                    if n.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_parse() {
        let r = Registry::builtin();
        assert!(r.harnesses.len() >= 10);
        for id in ["claude-code", "codex", "pi", "opencode", "hermes", "cursor-agent"] {
            assert_eq!(r.harness(id).unwrap().support, "full", "{id}");
        }
        for h in &r.harnesses {
            let id = &h.id;
            for (fid, f) in &h.files {
                assert!(["json", "jsonc", "json5", "toml", "yaml"].contains(&f.format.as_str()), "{id}.{fid}");
                assert!(["whole", "pointers", "record"].contains(&f.restore.as_str()), "{id}.{fid}");
            }
            // every `<file>#<pointer>` reference names a declared file
            let refs = h.model.default.iter().chain(&h.model.small).chain(&h.model.provider).chain(&h.model.base_url).chain(&h.mcp.sources).chain(&h.mcp.write);
            for r in refs {
                if let Some((file, _)) = r.split_once('#') {
                    assert!(h.files.contains_key(file), "{id}: {r}");
                }
            }
            for lp in &h.local_provider {
                assert!(h.files.contains_key(&lp.file), "{id}: local-provider file {}", lp.file);
                assert!(lp.runtimes.iter().all(|rt| r.runtime(rt).is_some()), "{id}: {:?}", lp.runtimes);
            }
        }
        // autonomous agents are agent CLIs, grouped apart in docs and `hlock harness list`
        for id in ["hermes", "openclaw", "nanobot", "picoclaw", "zeroclaw", "ironclaw"] {
            let h = r.harness(id).unwrap();
            assert!(h.autonomous && h.kind == "cli" && h.open_source == Some(true), "{id}");
        }
        assert_eq!(r.harness("clawdbot").unwrap().id, "openclaw");
        assert!(!r.harness("claude-code").unwrap().autonomous);
        for fw in ["pydantic-ai", "deepagents", "langgraph", "openai-agents", "claude-agent-sdk"] {
            let h = r.harness(fw).unwrap();
            assert_eq!(h.kind, "framework", "{fw}");
            assert!(h.packages.iter().all(|p| p.starts_with("pypi:")), "{fw}");
        }
        assert!(r.harnesses.iter().filter(|h| h.kind == "cli").count() >= 10);
        // `hlock run` needs every agent CLI's non-interactive mode
        for h in r.harnesses.iter().filter(|h| h.kind == "cli") {
            assert!(h.launch.prompt.iter().any(|a| a.contains("{prompt}")), "{}", h.id);
        }
        assert!(r.runtime("ollama").is_some());
        assert_eq!(r.harness("claude").unwrap().id, "claude-code");
        assert_eq!(r.runtime("lms").unwrap().id, "lmstudio");
    }

    #[test]
    fn versions() {
        assert_eq!(extract_version("2.1.285 (Claude Code)").as_deref(), Some("2.1.285"));
        assert_eq!(extract_version("Hermes Agent v0.20.0 (2026.8.3)").as_deref(), Some("0.20.0"));
        assert_eq!(extract_version("2026.05.28-a70ca7c").as_deref(), Some("2026.05.28-a70ca7c"));
        assert_eq!(extract_version("Warning: client version is 0.35.1").as_deref(), Some("0.35.1"));
        assert_eq!(extract_version("\u{1b}[38;5;166m art \u{1b}[0m\nCLI commit: 1.2.3").as_deref(), Some("1.2.3"));
        assert_eq!(extract_version("no version"), None);
    }

    #[test]
    fn install_detection() {
        let install = vec![InstallMethod {
            method: "script".into(),
            path_hints: vec!["/.local/share/cursor-agent/".into()],
            ..Default::default()
        }];
        let (m, p) = detect_install(Path::new("/opt/homebrew/Caskroom/claude-code/2.1.285/claude"), &[]);
        assert_eq!((m.as_deref(), p.as_deref()), (Some("brew-cask"), Some("claude-code")));
        let (m, p) = detect_install(Path::new("/opt/homebrew/Cellar/opencode/1.18.18/bin/opencode"), &[]);
        assert_eq!((m.as_deref(), p.as_deref()), (Some("brew"), Some("opencode")));
        let (m, _) = detect_install(Path::new("/home/u/.local/share/cursor-agent/versions/x/cursor-agent"), &install);
        assert_eq!(m.as_deref(), Some("script"));
        assert_eq!(source_spec("npm", Some("@openai/codex"), Some("0.5.0")), "npm:@openai/codex@0.5.0");
        assert_eq!(source_spec("pip", Some("vllm"), Some("0.11.2")), "pypi:vllm==0.11.2");
    }

    fn ctx_with(env: &[(&str, &str)]) -> Ctx {
        Ctx {
            home: PathBuf::from("/home/u"),
            cwd: PathBuf::from("/work"),
            env: env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            path: vec![],
        }
    }

    #[test]
    fn config_roots_and_isolation() {
        let r = Registry::builtin();
        let claude = r.harness("claude-code").unwrap();
        let ctx = ctx_with(&[]);
        assert_eq!(claude.config_root(&ctx), PathBuf::from("/home/u/.claude"));
        assert_eq!(claude.file_path("state", &ctx).unwrap(), PathBuf::from("/home/u/.claude.json"));
        assert_eq!(claude.file_path("project-mcp", &ctx).unwrap(), PathBuf::from("/work/.mcp.json"));
        let iso = ctx_with(&[("CLAUDE_CONFIG_DIR", "/tmp/env/claude")]);
        assert_eq!(claude.file_path("state", &iso).unwrap(), PathBuf::from("/tmp/env/claude/.claude.json"));

        let oc = r.harness("opencode").unwrap();
        let xdg = ctx_with(&[("XDG_CONFIG_HOME", "/tmp/env/xdg")]);
        assert_eq!(oc.config_root(&xdg), PathBuf::from("/tmp/env/xdg/opencode"));
        let (var, val) = oc.isolation_env(Path::new("/tmp/env/xdg/opencode")).unwrap();
        assert_eq!((var.as_str(), val), ("XDG_CONFIG_HOME", PathBuf::from("/tmp/env/xdg")));
    }

    #[test]
    fn selection_precedence() {
        let r = Registry::builtin();
        let ctx = ctx_with(&[("CLAUDECODE", "1"), ("HLOCK_HARNESS", "pi")]);
        assert_eq!(r.select(Some("codex"), Some("opencode"), &ctx).unwrap().id, "codex");
        assert_eq!(r.select(None, Some("opencode"), &ctx).unwrap().id, "pi");
        let ctx = ctx_with(&[("CLAUDECODE", "1")]);
        assert_eq!(r.select(None, Some("opencode"), &ctx).unwrap().id, "opencode");
        let s = r.select(None, None, &ctx).unwrap();
        assert_eq!((s.id.as_str(), s.reason.as_str()), ("claude-code", "running inside it"));
        assert!(r.select(Some("nope"), None, &ctx).unwrap_err().to_string().contains("known:"));
    }

    #[test]
    fn project_harnesses() {
        let m = crate::manifest::Manifest::from_str(
            r#"
[target]
harness = "triage"

[harnesses.triage]
framework = "pydantic-ai"
entrypoint = "triage.agent:run"

[harnesses.loop]
command = ["./bin/agent", "{prompt}"]
packages = ["pypi:httpx"]
"#,
        )
        .unwrap();
        let mut r = Registry::builtin();
        r.add_project_harnesses(&m).unwrap();
        let t = r.harness("triage").unwrap();
        assert_eq!(t.kind, "custom");
        assert_eq!(t.run.entrypoint.as_deref(), Some("triage.agent:run"));
        assert!(t.packages.contains(&"pypi:pydantic-ai-slim".to_string()));
        assert_eq!(r.harness("loop").unwrap().run.command[0], "./bin/agent");
        assert_eq!(r.select(None, m.target_harness().as_deref(), &ctx_with(&[])).unwrap().id, "triage");

        let bad = crate::manifest::Manifest::from_str("[harnesses.x]\nframework = \"nope\"\nentrypoint = \"a:b\"\n").unwrap();
        assert!(Registry::builtin().add_project_harnesses(&bad).unwrap_err().to_string().contains("not a known framework"));
        let bare = crate::manifest::Manifest::from_str("[harnesses.x]\nframework = \"langgraph\"\n").unwrap();
        assert!(Registry::builtin().add_project_harnesses(&bare).unwrap_err().to_string().contains("entrypoint"));
    }

    #[test]
    fn user_profiles_override_and_extend() {
        let dir = std::env::temp_dir().join(format!("hlock-prof-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("harnesses")).unwrap();
        std::fs::write(dir.join("harnesses/my.toml"), "id = \"my-agent\"\nbinaries = [\"my-agent\"]\n").unwrap();
        std::fs::write(dir.join("harnesses/pi.toml"), "id = \"pi\"\nname = \"Pi (fork)\"\n").unwrap();
        let mut r = Registry::builtin();
        r.extend_from_dir(&dir).unwrap();
        assert_eq!(r.harness("my-agent").unwrap().support, "basic");
        assert_eq!(r.harness("pi").unwrap().name, "Pi (fork)");
        assert_eq!(r.harnesses.iter().filter(|h| h.id == "pi").count(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn project_profiles_only_add_and_never_run_by_path() {
        let dir = std::env::temp_dir().join(format!("hlock-prof-proj-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let pd = dir.join("proj/.hlock/profiles");
        std::fs::create_dir_all(pd.join("harnesses")).unwrap();
        std::fs::create_dir_all(pd.join("runtimes")).unwrap();
        std::fs::create_dir_all(dir.join("home")).unwrap();
        std::fs::write(pd.join("harnesses/pi.toml"), "id = \"pi\"\nname = \"Pi (repo)\"\nbinaries = [\"./evil\"]\n").unwrap();
        std::fs::write(
            pd.join("harnesses/mine.toml"),
            "id = \"mine\"\nbinaries = [\"./evil\", \"mine\"]\nversion = { args = [\"-c\", \"touch pwned\"] }\n[run]\npython = \"./py\"\n",
        )
        .unwrap();
        std::fs::write(pd.join("runtimes/ollama.toml"), "id = \"ollama\"\nbinaries = [\"./evil\"]\n").unwrap();
        let ctx = Ctx { home: dir.join("home"), cwd: dir.join("proj"), env: BTreeMap::new(), path: vec![] };
        let r = Registry::load(&ctx).unwrap();
        // built-ins aren't replaced by the repository
        assert_eq!(r.harness("pi").unwrap().origin, "builtin");
        assert_eq!(r.runtime("ollama").unwrap().origin, "builtin");
        // a new profile is added, without path-named programs or a non-version probe
        let mine = r.harness("mine").unwrap();
        assert_eq!(mine.binaries, ["mine"]);
        assert!(mine.version.args.is_empty() && mine.run.python.is_none());
        assert_eq!(r.notes.len(), 5, "{:?}", r.notes);
        assert!(r.notes.iter().any(|n| n.contains("can't replace the harness profile \"pi\"")));
        // the same files in a user directory keep full override power
        let mut user = Registry::builtin();
        user.extend_from_dir(&pd).unwrap();
        assert_eq!(user.harness("pi").unwrap().name, "Pi (repo)");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn run_capture_is_none_on_failure() {
        let ctx = Ctx::from_process();
        let sh = ctx.which("sh").unwrap();
        let s = |a: &[&str]| a.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert!(run_capture(&sh, &s(&["-c", "echo 1.2.3"]), &ctx, VERSION_TIMEOUT).unwrap().contains("1.2.3"));
        assert!(run_capture(&sh, &s(&["-c", "echo 1.2.3; exit 1"]), &ctx, VERSION_TIMEOUT).is_none());
        // more than a pipe holds, on both streams, doesn't block
        let big = run_capture(&sh, &s(&["-c", "head -c 200000 /dev/zero; head -c 200000 /dev/zero >&2"]), &ctx, VERSION_TIMEOUT).unwrap();
        assert_eq!(big.len(), 400_001);
    }
}
