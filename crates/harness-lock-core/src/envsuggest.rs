//! What a lock runs that `[environment]` doesn't declare: the programs its MCP servers,
//! hooks and settings start, and the ones its skills' scripts and instructions call, each
//! with the packages that provide it (`hlock env suggest`, and the meta harness's
//! `environment` job).
//!
//! It suggests; it doesn't decide. A command a lock runs as written is `high` confidence,
//! one found in a script or a code block `low` (it may be an example). Package names come
//! from this machine's package manager where it knows the program, else from a short
//! table of common tools.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::error::{Error, Result};
use crate::lockfile::Lockfile;
use crate::manifest::Environment;
use crate::profiles::{Ctx, HarnessProfile};

/// A program the stack runs, and what would install it.
#[derive(Debug, Clone, Serialize, Default, PartialEq)]
pub struct Candidate {
    pub program: String,
    /// `high`: a command the lock runs as written (an MCP server, a hook, a setting).
    /// `low`: one found in a skill's scripts, a hook's script, or a code block.
    pub confidence: String,
    /// Where it's run: `mcp github`, `hook PreToolUse Bash`, `skill release: scripts/pr.sh`.
    pub sources: Vec<String>,
    /// The package that provides it, by installer: `{"apt": "ripgrep", "brew": "ripgrep"}`.
    pub packages: BTreeMap<String, String>,
    /// Where it is on this machine's `PATH`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub found: Option<String>,
    /// `declared` (`[environment]` has one of its packages), `ignored`, or `missing`.
    pub status: String,
}

/// Programs every machine (and image) the agent runs on has: shell builtins and
/// keywords, and the POSIX and coreutils tools.
const BASE: &[&str] = &[
    "sh", "bash", "zsh", "dash", "env", "cd", "echo", "printf", "cat", "grep", "egrep", "fgrep", "sed", "awk", "test", "[", "[[", "true", "false",
    "exit", "export", "set", "unset", "if", "then", "else", "elif", "fi", "for", "while", "until", "do", "done", "case", "esac", "in",
    "function", "return", "local", "source", ".", "exec", "eval", "read", "shift", "trap", "wait", "sleep", "date", "basename", "dirname",
    "mkdir", "rmdir", "rm", "cp", "mv", "ln", "ls", "head", "tail", "sort", "uniq", "wc", "tr", "cut", "xargs", "find", "chmod", "chown",
    "touch", "tee", "which", "command", "type", "pwd", "printenv", "kill", "ps", "tar", "gzip", "gunzip", "mktemp", "realpath", "readlink",
    "stat", "diff", "cmp", "id", "whoami", "uname", "hostname", "time", "nohup", "timeout", "seq", "yes", "expr", "let", "declare",
    "typeset", "alias", "sudo", "{", "}", "!", "(", ")", "break", "continue", "getopts", "umask", "ulimit", "hash", "builtin", "pushd", "popd",
];

/// What provides common programs, where their package names differ or no package
/// manager on this machine says (`dpkg -S` on Linux, Homebrew's Cellar on macOS).
const KNOWN: &[(&str, &[(&str, &str)])] = &[
    ("node", &[("apt", "nodejs"), ("brew", "node")]),
    ("npm", &[("apt", "npm"), ("brew", "node")]),
    ("npx", &[("apt", "npm"), ("brew", "node")]),
    ("pnpm", &[("brew", "pnpm"), ("npm", "pnpm")]),
    ("yarn", &[("brew", "yarn"), ("npm", "yarn")]),
    ("bun", &[("npm", "bun")]),
    ("bunx", &[("npm", "bun")]),
    ("deno", &[("brew", "deno")]),
    ("tsx", &[("npm", "tsx")]),
    ("uv", &[("brew", "uv"), ("pip", "uv")]),
    ("uvx", &[("brew", "uv"), ("pip", "uv")]),
    ("python3", &[("apt", "python3"), ("brew", "python")]),
    ("python", &[("apt", "python-is-python3"), ("brew", "python")]),
    ("pip", &[("apt", "python3-pip"), ("brew", "python")]),
    ("pip3", &[("apt", "python3-pip"), ("brew", "python")]),
    ("pipx", &[("apt", "pipx"), ("brew", "pipx")]),
    ("ruff", &[("brew", "ruff"), ("pip", "ruff")]),
    ("black", &[("pip", "black")]),
    ("pytest", &[("pip", "pytest")]),
    ("mypy", &[("pip", "mypy")]),
    ("eslint", &[("npm", "eslint")]),
    ("prettier", &[("npm", "prettier")]),
    ("docker", &[("apt", "docker.io"), ("brew", "docker")]),
    ("git", &[("apt", "git"), ("brew", "git")]),
    ("gh", &[("apt", "gh"), ("brew", "gh")]),
    ("rg", &[("apt", "ripgrep"), ("brew", "ripgrep")]),
    ("fd", &[("apt", "fd-find"), ("brew", "fd")]),
    ("fdfind", &[("apt", "fd-find")]),
    ("jq", &[("apt", "jq"), ("brew", "jq")]),
    ("yq", &[("apt", "yq"), ("brew", "yq")]),
    ("curl", &[("apt", "curl"), ("brew", "curl")]),
    ("wget", &[("apt", "wget"), ("brew", "wget")]),
    ("zip", &[("apt", "zip"), ("brew", "zip")]),
    ("unzip", &[("apt", "unzip"), ("brew", "unzip")]),
    ("make", &[("apt", "make"), ("brew", "make")]),
    ("cmake", &[("apt", "cmake"), ("brew", "cmake")]),
    ("protoc", &[("apt", "protobuf-compiler"), ("brew", "protobuf")]),
    ("go", &[("apt", "golang-go"), ("brew", "go")]),
    ("gofmt", &[("apt", "golang-go"), ("brew", "go")]),
    ("cargo", &[("apt", "cargo"), ("brew", "rust")]),
    ("rustc", &[("apt", "rustc"), ("brew", "rust")]),
    ("ruby", &[("apt", "ruby"), ("brew", "ruby")]),
    ("sqlite3", &[("apt", "sqlite3"), ("brew", "sqlite")]),
    ("psql", &[("apt", "postgresql-client"), ("brew", "libpq")]),
    ("ffmpeg", &[("apt", "ffmpeg"), ("brew", "ffmpeg")]),
    ("pandoc", &[("apt", "pandoc"), ("brew", "pandoc")]),
    ("aws", &[("apt", "awscli"), ("brew", "awscli")]),
    ("kubectl", &[("brew", "kubernetes-cli")]),
    ("helm", &[("brew", "helm")]),
];

/// Settings that are a command line the harness runs (see `trust::setting_executables`).
const COMMAND_SETTINGS: &[&str] = &[
    "status line command", "API key helper", "AWS auth refresh", "AWS credential export", "OpenTelemetry headers helper",
    "notify command", "tool discovery command", "tool call command",
];

/// The most of a script or instructions file read.
const MAX_READ: usize = 1 << 20;

/// What `lock` runs, as candidates against `env` (the manifest's `[environment]`), most
/// certain first. `lock_dir` resolves the lock's relative paths.
pub fn suggest(lock: &Lockfile, lock_dir: &Path, env: &Environment, profile: Option<&HarnessProfile>, ctx: &Ctx) -> Vec<Candidate> {
    let mut found: BTreeMap<String, (bool, Vec<String>)> = BTreeMap::new();
    let mut add = |program: String, high: bool, source: String| {
        let e = found.entry(program).or_insert((false, Vec::new()));
        e.0 |= high;
        if !e.1.contains(&source) {
            e.1.push(source);
        }
    };
    // command lines the lock runs as written: `trust` reads the vendored config, and a
    // project's own config files (not vendored: the repository has them) are read in place
    let mut runs = crate::trust::executables(lock, lock_dir, profile, ctx);
    for c in lock.config.iter().filter(|c| c.vendored.is_none() && crate::fsutil::safe_rel(&c.path)) {
        let fmt = if c.format == "jsonc" { "json" } else { c.format.as_str() };
        let Ok(Some(doc)) = crate::capture::read_config(&lock_dir.join(&c.path), fmt) else { continue };
        let view = |ptr: &str| doc.pointer(ptr).cloned();
        runs.extend(crate::trust::hook_executables(&doc, &c.id));
        runs.extend(crate::trust::setting_executables(&view, &c.id));
    }
    for e in runs {
        let source = match e.kind.as_str() {
            "mcp" if !e.detail.contains("://") => format!("mcp {}", e.name),
            "hook" => format!("hook {}", e.name),
            "setting" if COMMAND_SETTINGS.contains(&e.name.as_str()) => format!("setting {}", e.name),
            _ => continue,
        };
        for p in line_programs(&e.detail) {
            add(p, true, source.clone());
        }
        // a script the line runs: its interpreter and what it calls
        for path in line_scripts(&e.detail, lock_dir, ctx) {
            if let Ok(text) = crate::fsutil::read_regular(&path, MAX_READ).map(|b| String::from_utf8_lossy(&b).into_owned()) {
                let shown = format!("{source}: {}", show(&path, lock_dir, ctx));
                for p in script_programs(&text, &path.to_string_lossy()) {
                    add(p, false, shown.clone());
                }
            }
        }
    }
    // what the skills', helper scripts' and instructions' content calls
    for c in crate::audit::contents(lock, lock_dir, profile, ctx) {
        let files: Vec<(PathBuf, String)> = match c.kind.as_str() {
            "skill" => std::iter::once("SKILL.md".to_string())
                .chain(crate::capture::skill_scripts(&c.path).into_iter().map(|f| format!("scripts/{f}")))
                .map(|f| (c.path.join(&f), f))
                .collect(),
            "script" | "instructions" => vec![(c.path.clone(), c.label.clone())],
            _ => continue,
        };
        for (path, rel) in files {
            let Ok(bytes) = crate::fsutil::read_regular(&path, MAX_READ) else { continue };
            let text = String::from_utf8_lossy(&bytes);
            let programs = if rel.ends_with(".md") { markdown_programs(&text) } else { script_programs(&text, &rel) };
            let source = match c.kind.as_str() {
                "skill" => format!("skill {}: {rel}", c.name),
                k => format!("{k} {rel}"),
            };
            for p in programs {
                add(p, false, source.clone());
            }
        }
    }
    // the harness itself and hlock are installed by `sync`, not the environment
    let own: BTreeSet<String> = ["hlock", "harness-lock", lock.harness.name.as_str()]
        .into_iter()
        .map(str::to_string)
        .chain(profile.into_iter().flat_map(|p| p.binaries.iter().cloned()))
        .collect();
    let mut out: Vec<Candidate> = found
        .into_iter()
        .filter(|(p, _)| !own.contains(p))
        .map(|(program, (high, sources))| {
            let at = ctx.which(&program);
            let mut packages = host_packages(at.as_deref(), ctx);
            if let Some((_, known)) = KNOWN.iter().find(|(p, _)| *p == program) {
                for (installer, pkg) in *known {
                    packages.entry(installer.to_string()).or_insert_with(|| pkg.to_string());
                }
            }
            let status = if env.ignore.contains(&program) {
                "ignored"
            } else if declared(env, &program, &packages) {
                "declared"
            } else {
                "missing"
            };
            Candidate { program, confidence: if high { "high" } else { "low" }.into(), sources, packages, found: at.map(|p| p.to_string_lossy().into_owned()), status: status.into() }
        })
        .collect();
    out.sort_by(|a, b| (a.confidence != "high", &a.program).cmp(&(b.confidence != "high", &b.program)));
    out
}

/// [`suggest`] for the lock at `lock_path`, against `manifest`'s `[environment]` (none
/// declared without one).
pub fn for_lock(lock_path: &Path, manifest: Option<&crate::manifest::Manifest>, reg: &crate::profiles::Registry, ctx: &Ctx) -> Result<Vec<Candidate>> {
    let lock = Lockfile::load(lock_path)?;
    let dir = lock_path.parent().filter(|d| !d.as_os_str().is_empty()).map(Path::to_path_buf).unwrap_or_else(|| ctx.cwd.clone());
    let env = manifest.map(|m| m.environment.clone()).unwrap_or_default();
    Ok(suggest(&lock, &dir, &env, reg.harness(&lock.harness.name), ctx))
}

/// One line for a candidate: `rg  high  hook PreToolUse Bash  apt ripgrep, brew ripgrep`.
pub fn describe(c: &Candidate) -> String {
    let pkgs = if c.packages.is_empty() { "no package known".to_string() } else { c.packages.iter().map(|(i, p)| format!("{i} {p}")).collect::<Vec<_>>().join(", ") };
    format!("{} ({}, {}): {}. Packages: {pkgs}.{}", c.program, c.confidence, c.status, c.sources.join("; "), c.found.as_deref().map(|f| format!(" Found at {f}.")).unwrap_or_default())
}

fn show(p: &Path, lock_dir: &Path, ctx: &Ctx) -> String {
    if let Ok(r) = p.strip_prefix(lock_dir) {
        return r.to_string_lossy().into_owned();
    }
    match p.strip_prefix(&ctx.home) {
        Ok(r) => format!("~/{}", r.to_string_lossy()),
        Err(_) => p.to_string_lossy().into_owned(),
    }
}

/// Whether `[environment]` provides `program`: a list has one of its packages (or a
/// package of its name), or a binary is installed under its name.
fn declared(env: &Environment, program: &str, packages: &BTreeMap<String, String>) -> bool {
    env.lists().iter().any(|(installer, items)| {
        items.iter().map(|i| package_name(installer, i)).any(|n| n == program || packages.get(*installer).is_some_and(|p| *p == n))
    }) || env.binaries.iter().any(|b| Path::new(&b.path).file_name().is_some_and(|f| f == program))
}

/// A list entry's package, without its version: `ruff==0.6.9` → `ruff`, `pnpm@9` →
/// `pnpm`, `@scope/pkg@1` → `@scope/pkg`, `golang.org/x/tools/gopls@v0.16.2` → the path.
pub fn package_name(installer: &str, entry: &str) -> String {
    let e = entry.trim();
    match installer {
        "pip" => e.split(['=', '<', '>', '~', '!', '[', ';', ' ']).next().unwrap_or(e).to_lowercase(),
        "npm" | "go" => match e.rfind('@') {
            Some(i) if i > 0 => e[..i].to_string(),
            _ => e.to_string(),
        },
        "apt" => e.split('=').next().unwrap_or(e).to_string(),
        _ => e.split('@').next().unwrap_or(e).to_string(),
    }
}

/// What this machine's package manager says provides `program`: the Homebrew formula,
/// npm package or uv / pipx tool its path resolves into, or `dpkg -S` on Debian.
fn host_packages(at: Option<&Path>, ctx: &Ctx) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let Some(at) = at else { return out };
    let real = std::fs::canonicalize(at).unwrap_or_else(|_| at.to_path_buf());
    let s = real.to_string_lossy().replace('\\', "/");
    let after = |marker: &str| s.split_once(marker).and_then(|(_, rest)| rest.split('/').next()).filter(|n| !n.is_empty()).map(str::to_string);
    if let Some(f) = after("/Cellar/") {
        out.insert("brew".into(), f);
    }
    if let Some((_, rest)) = s.split_once("/node_modules/") {
        let mut parts = rest.split('/');
        let pkg = match parts.next() {
            Some(scope) if scope.starts_with('@') => parts.next().map(|n| format!("{scope}/{n}")),
            other => other.map(str::to_string),
        };
        if let Some(p) = pkg.filter(|p| !p.is_empty() && p != "npm") {
            out.insert("npm".into(), p);
        }
    }
    if let Some(t) = after("/uv/tools/").or_else(|| after("/pipx/venvs/")) {
        out.insert("pip".into(), t);
    }
    if out.is_empty() && ctx.os() == "linux" && ctx.which("dpkg").is_some() {
        let mut c = std::process::Command::new("dpkg");
        c.args(["-S", &real.to_string_lossy()]).env_clear().envs(&ctx.env).stdin(std::process::Stdio::null());
        if let Ok(o) = crate::process::run(c, &crate::process::Limits::timeout(std::time::Duration::from_secs(10))) {
            let text = o.stdout_text();
            if let Some(pkg) = o.success.then(|| text.lines().next().and_then(|l| l.split_once(": ")).map(|(p, _)| p.split(':').next().unwrap_or(p).trim().to_string())).flatten() {
                if crate::fsutil::plain_token(&pkg) {
                    out.insert("apt".into(), pkg);
                }
            }
        }
    }
    out
}

/// The programs a command line starts: the first word of each command in it, past
/// `VAR=value`, `env`, `sudo` and the like. Paths into the project (`./server.js`) aren't
/// programs; a system path's (`/usr/bin/python3`) is its file name.
pub fn line_programs(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let flat = line.replace("&&", "\n").replace("||", "\n").replace("$(", "\n").replace(['|', ';', '`', '(', ')'], "\n");
    for seg in flat.lines() {
        let mut words = seg.split_whitespace().map(|w| w.trim_matches(|c| c == '"' || c == '\''));
        let mut prog = None;
        let mut takes_value = false;
        for w in words.by_ref() {
            // `env -u VAR`, `sudo -u user`, `env -C dir`: the option's value isn't the program
            if std::mem::take(&mut takes_value) {
                continue;
            }
            if matches!(w, "-u" | "-C" | "-g" | "--unset" | "--chdir" | "--user") {
                takes_value = true;
                continue;
            }
            let assignment = w.split_once('=').is_some_and(|(k, _)| crate::sandbox::valid_env_name(k));
            if assignment || w.starts_with('-') || matches!(w, "env" | "sudo" | "exec" | "time" | "nohup" | "command" | "then" | "do" | "else" | "!") {
                continue;
            }
            prog = Some(w);
            break;
        }
        if let Some(p) = prog.and_then(program_name) {
            if !out.contains(&p) && !BASE.contains(&p.as_str()) {
                out.push(p);
            }
        }
    }
    out
}

/// A command word as a program name, if it is one.
fn program_name(w: &str) -> Option<String> {
    if w.contains('$') || w.contains('{') || w.contains('*') {
        return None;
    }
    let name = if w.contains('/') {
        const SYSTEM: [&str; 6] = ["/usr/bin/", "/usr/local/bin/", "/bin/", "/usr/sbin/", "/sbin/", "/opt/homebrew/bin/"];
        if !SYSTEM.iter().any(|s| w.starts_with(s)) {
            return None;
        }
        w.rsplit('/').next().unwrap_or(w)
    } else {
        w
    };
    let ok = !name.is_empty() && !name.starts_with(['-', '.']) && name.chars().all(|c| c.is_ascii_alphanumeric() || "._+-".contains(c)) && name.chars().any(|c| c.is_ascii_alphabetic());
    ok.then(|| name.to_string())
}

/// The scripts a command line runs that exist here (see `trust::referenced_files`).
fn line_scripts(line: &str, lock_dir: &Path, ctx: &Ctx) -> Vec<PathBuf> {
    let project = lock_dir.to_string_lossy();
    let home = ctx.home.to_string_lossy();
    // each command of the line (`jq … | rg …; ./hooks/guard.sh`), not only the first
    let flat = line.replace("&&", "\n").replace("||", "\n").replace(['|', ';'], "\n");
    let toks: BTreeSet<String> = flat.lines().flat_map(|seg| crate::trust::referenced_files(seg.trim(), lock_dir, ctx).into_keys()).collect();
    toks.into_iter()
        .map(|tok| {
            let t = tok.replace("${CLAUDE_PROJECT_DIR}", &project).replace("$CLAUDE_PROJECT_DIR", &project).replace("${HOME}", &home).replace("$HOME", &home);
            match t.strip_prefix("~/") {
                Some(r) => ctx.home.join(r),
                None if t.starts_with('/') => PathBuf::from(t),
                None => lock_dir.join(t),
            }
        })
        .collect()
}

/// The programs a script calls: its interpreter, every command of a shell script, and
/// the programs a Python or JavaScript one runs (`subprocess.run(["git", …])`,
/// `execSync("gh …")`, `shutil.which("rg")`).
pub fn script_programs(text: &str, name: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let push = |p: String, out: &mut Vec<String>| {
        if !out.contains(&p) && !BASE.contains(&p.as_str()) {
            out.push(p);
        }
    };
    let first = text.lines().next().unwrap_or("");
    let interp = first.strip_prefix("#!").map(|s| {
        let mut w = s.split_whitespace();
        let a = w.next().unwrap_or("");
        let b = if a.ends_with("/env") { w.find(|x| !x.starts_with('-')).unwrap_or("") } else { a };
        b.rsplit('/').next().unwrap_or(b).to_string()
    });
    if let Some(i) = interp.clone().filter(|i| !i.is_empty()) {
        push(i, &mut out);
    }
    let ext = Path::new(name).extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    let shell = matches!(ext.as_str(), "sh" | "bash" | "zsh") || interp.as_deref().is_some_and(|i| matches!(i, "sh" | "bash" | "zsh" | "dash"));
    if shell {
        // functions the script defines aren't programs
        let defined: BTreeSet<String> = text
            .lines()
            .filter_map(|l| l.trim().trim_start_matches("function ").split_once("()").map(|(n, _)| n.trim().to_string()))
            .filter(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_alphanumeric() || "_-".contains(c)))
            .collect();
        for line in text.lines().skip(usize::from(interp.is_some())) {
            let l = line.trim();
            if l.is_empty() || l.starts_with('#') {
                continue;
            }
            for p in line_programs(l).into_iter().filter(|p| !defined.contains(p)) {
                push(p, &mut out);
            }
        }
        return out;
    }
    const CALLS: [&str; 12] = ["run(", "call(", "check_call(", "check_output(", "Popen(", "which(", "execSync(", "exec(", "execFile(", "execFileSync(", "spawn(", "spawnSync("];
    for call in CALLS {
        for (i, _) in text.match_indices(call) {
            // a method or function of that name: `subprocess.run(`, not `rerun(`
            if text[..i].chars().next_back().is_some_and(|c| c.is_ascii_alphanumeric() || c == '_') {
                continue;
            }
            let rest = text[i + call.len()..].trim_start();
            let rest = rest.strip_prefix('[').map(str::trim_start).unwrap_or(rest);
            let Some(q) = rest.chars().next().filter(|c| matches!(c, '"' | '\'' | '`')) else { continue };
            let body = &rest[1..];
            let Some(end) = body.find(q) else { continue };
            if let Some(p) = line_programs(&body[..end]).into_iter().next() {
                push(p, &mut out);
            }
        }
    }
    out
}

/// The programs a Markdown file's shell code blocks run (```` ```bash ````, `sh`, `shell`,
/// `zsh`, and `console`'s `$ ` lines).
pub fn markdown_programs(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut block: Option<String> = None;
    for line in text.lines() {
        let t = line.trim_start();
        if let Some(fence) = t.strip_prefix("```") {
            block = match block {
                Some(_) => None,
                None => Some(fence.split_whitespace().next().unwrap_or("").to_lowercase()),
            };
            continue;
        }
        let cmd = match block.as_deref() {
            Some("bash" | "sh" | "shell" | "zsh") if !t.starts_with('#') => t,
            Some("console") => match t.strip_prefix("$ ") {
                Some(c) => c,
                None => continue,
            },
            _ => continue,
        };
        for p in line_programs(cmd) {
            if !out.contains(&p) {
                out.push(p);
            }
        }
    }
    out
}

/// The installers `--write` adds a package to: `apt` and `brew` when both have it, else a
/// language installer that has it everywhere (`uv` is `pip`'s, not only `brew`'s), else
/// the system installers that do.
pub fn installers_for(c: &Candidate) -> Vec<(String, String)> {
    let pick = |names: &[&str]| -> Vec<(String, String)> { c.packages.iter().filter(|(i, _)| names.contains(&i.as_str())).map(|(i, p)| (i.clone(), p.clone())).collect() };
    let system = pick(&["apt", "brew"]);
    if system.len() == 2 {
        return system;
    }
    let language: Vec<(String, String)> = pick(&["pip", "npm", "cargo", "go"]).into_iter().take(1).collect();
    if language.is_empty() { system } else { language }
}

/// Add the missing candidates (`high` ones, or every one with `all`) to `harness.toml`'s
/// `[environment]`, keeping the file's comments and layout. Returns the lines added
/// (`apt: ripgrep`), and the programs with no package to add.
pub fn write(manifest: &Path, candidates: &[Candidate], all: bool) -> Result<(Vec<String>, Vec<String>)> {
    let text = std::fs::read_to_string(manifest).map_err(|e| crate::error::io_at(manifest, e))?;
    let mut doc = text.parse::<toml_edit::DocumentMut>().map_err(|e| Error::Manifest(format!("invalid {}: {e}", manifest.display())))?;
    let mut added = Vec::new();
    let mut unknown = Vec::new();
    for c in candidates.iter().filter(|c| c.status == "missing" && (all || c.confidence == "high")) {
        let to = installers_for(c);
        if to.is_empty() {
            unknown.push(c.program.clone());
            continue;
        }
        for (installer, pkg) in to {
            if doc.get("environment").is_none() {
                let mut t = toml_edit::Table::new();
                t.set_implicit(false);
                doc["environment"] = toml_edit::Item::Table(t);
            }
            let env = doc["environment"].as_table_like_mut().ok_or_else(|| Error::Manifest("[environment] in harness.toml isn't a table".into()))?;
            if env.get(&installer).is_none() {
                env.insert(&installer, toml_edit::value(toml_edit::Array::new()));
            }
            let arr = env.get_mut(&installer).and_then(toml_edit::Item::as_array_mut).ok_or_else(|| Error::Manifest(format!("[environment] {installer} in harness.toml isn't a list")))?;
            if !arr.iter().filter_map(toml_edit::Value::as_str).any(|e| package_name(&installer, e) == pkg) {
                arr.push(pkg.as_str());
                added.push(format!("{installer}: {pkg}"));
            }
        }
    }
    if !added.is_empty() {
        // a manifest that doesn't load afterwards isn't written
        let out = doc.to_string();
        crate::manifest::Manifest::from_str(&out)?.check_declared()?;
        crate::fsutil::write_atomic(manifest, out).map_err(|e| crate::error::at(manifest, e))?;
    }
    Ok((added, unknown))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_lines_and_scripts() {
        assert_eq!(line_programs("npx -y @modelcontextprotocol/server-github"), ["npx"]);
        assert_eq!(line_programs("FOO=1 env -u X uvx mcp-server-fetch"), ["uvx"]);
        assert_eq!(line_programs("jq -r .tool_input.command | rg -q 'rm -rf' && echo blocked"), ["jq", "rg"]);
        assert_eq!(line_programs("/usr/bin/python3 ./server.py; ./bin/x; $HOME/y"), ["python3"]);
        assert_eq!(line_programs("sudo docker run --rm img"), ["docker"]);
        assert!(line_programs("cd \"$CLAUDE_PROJECT_DIR\" && test -f x").is_empty());

        let sh = "#!/usr/bin/env bash\nset -e\nlog() { echo \"$1\"; }\n# gh is mentioned here only\nlog start\ngh pr create --fill\nprotoc --version | head -1\n";
        assert_eq!(script_programs(sh, "scripts/pr.sh"), ["gh", "protoc"]);
        let py = "#!/usr/bin/env python3\nimport subprocess, shutil\nsubprocess.run([\"git\", \"status\"])\nshutil.which('rg')\nrerun(\"nope\")\nsubprocess.check_output(\"ffmpeg -i a b\", shell=True)\n";
        assert_eq!(script_programs(py, "x.py"), ["python3", "git", "ffmpeg", "rg"], "in the order the call patterns are tried");
        let js = "const { execSync } = require('child_process');\nexecSync(`pandoc a.md -o a.pdf`);\n";
        assert_eq!(script_programs(js, "x.js"), ["pandoc"]);

        let md = "Run this:\n\n```bash\n# a comment\nuv run pytest\n```\n\n```console\n$ cargo test\nok\n```\n\n```python\nimport os\n```\n";
        assert_eq!(markdown_programs(md), ["uv", "cargo"]);
    }

    #[test]
    fn package_names_without_versions() {
        for (installer, entry, want) in [("pip", "ruff==0.6.9", "ruff"), ("pip", "Black[d]>=24", "black"), ("npm", "pnpm@9", "pnpm"), ("npm", "@scope/pkg@1.2", "@scope/pkg"), ("npm", "@scope/pkg", "@scope/pkg"), ("go", "golang.org/x/tools/gopls@v0.16.2", "golang.org/x/tools/gopls"), ("apt", "ripgrep=13.0.0-4", "ripgrep"), ("brew", "ripgrep", "ripgrep")] {
            assert_eq!(package_name(installer, entry), want, "{installer} {entry}");
        }
    }

    #[test]
    fn host_packages_from_install_paths() {
        let d = std::env::temp_dir().join(format!("hlock-envsuggest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let ctx = Ctx { home: d.clone(), cwd: d.clone(), env: Default::default(), path: vec![d.join("bin")] };
        for (rel, installer, pkg) in [("Cellar/ripgrep/14.1.0/bin/rg", "brew", "ripgrep"), ("lib/node_modules/@biomejs/biome/bin/biome", "npm", "@biomejs/biome"), (".local/share/uv/tools/ruff/bin/ruff", "pip", "ruff")] {
            let p = d.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, "").unwrap();
            assert_eq!(host_packages(Some(&p), &ctx).get(installer).map(String::as_str), Some(pkg), "{rel}");
        }
        assert!(host_packages(None, &ctx).is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    fn project(tag: &str) -> (PathBuf, Ctx, Lockfile) {
        let d = std::env::temp_dir().join(format!("hlock-envsuggest-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join(".claude")).unwrap();
        std::fs::write(d.join(".claude/settings.json"), r#"{"hooks": {"PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "jq -r .x | rg -q y; ./hooks/guard.sh"}]}]}}"#).unwrap();
        std::fs::create_dir_all(d.join("hooks")).unwrap();
        std::fs::write(d.join("hooks/guard.sh"), "#!/bin/sh\nshellcheck \"$1\"\n").unwrap();
        let mut lock = Lockfile { kind: Some("pin".into()), ..Default::default() };
        lock.harness.name = "claude-code".into();
        lock.tool.push(crate::lockfile::Tool { name: "github".into(), kind: "mcp".into(), transport: Some("stdio".into()), command: Some("npx".into()), args: vec!["-y".into(), "@modelcontextprotocol/server-github".into()], ..Default::default() });
        lock.tool.push(crate::lockfile::Tool { name: "remote".into(), kind: "mcp".into(), transport: Some("http".into()), url: Some("https://mcp.example.com".into()), ..Default::default() });
        lock.config.push(crate::lockfile::ConfigFile { id: "project-settings".into(), path: ".claude/settings.json".into(), format: "json".into(), ..Default::default() });
        (d.clone(), Ctx { home: d.join("home"), cwd: d.clone(), env: Default::default(), path: Vec::new() }, lock)
    }

    #[test]
    fn suggest_reads_mcp_servers_hooks_and_their_scripts() {
        let (d, ctx, lock) = project("suggest");
        let env = Environment { apt: vec!["jq".into()], ignore: vec!["shellcheck".into()], ..Default::default() };
        let found = suggest(&lock, &d, &env, None, &ctx);
        let row = |p: &str| found.iter().find(|c| c.program == p).unwrap_or_else(|| panic!("{p} not in {found:?}"));
        assert_eq!(found.iter().map(|c| c.program.as_str()).collect::<Vec<_>>(), ["jq", "npx", "rg", "shellcheck"], "high first, the remote server not at all");
        assert_eq!((row("npx").confidence.as_str(), row("npx").status.as_str()), ("high", "missing"));
        assert_eq!(row("npx").sources, ["mcp github"]);
        assert_eq!(row("npx").packages.get("apt").map(String::as_str), Some("npm"));
        assert_eq!(row("jq").status, "declared");
        assert_eq!(row("rg").sources, ["hook PreToolUse Bash"]);
        let sc = row("shellcheck");
        assert_eq!((sc.confidence.as_str(), sc.status.as_str(), sc.sources[0].as_str()), ("low", "ignored", "hook PreToolUse Bash: hooks/guard.sh"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn write_adds_packages_and_keeps_the_file() {
        let (d, ctx, lock) = project("write");
        let manifest = d.join("harness.toml");
        std::fs::write(&manifest, "# the release bot\n[project]\nname = \"bot\"\n\n[environment]\napt = [\"jq\"]  # for the hook\n").unwrap();
        let m = crate::manifest::Manifest::load(&manifest).unwrap();
        let found = suggest(&lock, &d, &m.environment, None, &ctx);
        let (added, unknown) = write(&manifest, &found, false).unwrap();
        assert_eq!(added, ["apt: npm", "brew: node", "apt: ripgrep", "brew: ripgrep"]);
        assert!(unknown.is_empty(), "{unknown:?}");
        let text = std::fs::read_to_string(&manifest).unwrap();
        assert!(text.starts_with("# the release bot\n") && text.contains("# for the hook"), "{text}");
        let m = crate::manifest::Manifest::load(&manifest).unwrap();
        assert_eq!((m.environment.apt.clone(), m.environment.brew.clone()), (vec!["jq".to_string(), "npm".into(), "ripgrep".into()], vec!["node".to_string(), "ripgrep".into()]));
        // again: nothing left to add; with --all, the low-confidence one has no package known
        let found = suggest(&lock, &d, &m.environment, None, &ctx);
        assert!(write(&manifest, &found, false).unwrap().0.is_empty());
        assert_eq!(write(&manifest, &found, true).unwrap(), (Vec::<String>::new(), vec!["shellcheck".to_string()]));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn installers_prefer_both_platforms_then_a_language_installer() {
        let c = |pairs: &[(&str, &str)]| Candidate { packages: pairs.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect(), ..Default::default() };
        let names = |c: &Candidate| installers_for(c).into_iter().map(|(i, p)| format!("{i} {p}")).collect::<Vec<_>>();
        assert_eq!(names(&c(&[("apt", "ripgrep"), ("brew", "ripgrep")])), ["apt ripgrep", "brew ripgrep"]);
        assert_eq!(names(&c(&[("brew", "uv"), ("pip", "uv")])), ["pip uv"], "brew alone would leave Linux out");
        assert_eq!(names(&c(&[("brew", "deno")])), ["brew deno"]);
        assert!(names(&c(&[])).is_empty());
    }

    #[test]
    fn declared_by_package_program_or_binary() {
        let env = Environment { apt: vec!["ripgrep".into()], npm: vec!["pnpm@9".into()], ..Default::default() };
        let pk = |pairs: &[(&str, &str)]| pairs.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect::<BTreeMap<_, _>>();
        assert!(declared(&env, "rg", &pk(&[("apt", "ripgrep"), ("brew", "ripgrep")])));
        assert!(declared(&env, "pnpm", &pk(&[])), "a package of the program's name");
        assert!(!declared(&env, "jq", &pk(&[("apt", "jq")])));
        let bin = Environment { binaries: vec![crate::manifest::Binary { url: "https://x/t".into(), sha256: "0".repeat(64), path: "/usr/local/bin/tool".into() }], ..Default::default() };
        assert!(declared(&bin, "tool", &pk(&[])));
    }
}
