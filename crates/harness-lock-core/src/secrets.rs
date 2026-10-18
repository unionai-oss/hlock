//! Where secret values come from (`specs/secrets.md`).
//!
//! A lock never holds a secret: `pin` writes `${HLOCK_SECRET_…}` placeholders.
//! [`lookup`] finds a placeholder's value, first hit wins:
//!
//! 1. the environment variable of the same name;
//! 2. a reference in the user's secrets file (`~/.config/hlock/secrets.toml`, or
//!    `HLOCK_SECRETS_FILE`): a command to run, another variable, or a file;
//! 3. the OS keychain (macOS `security`, Linux `secret-tool`), service `hlock`,
//!    account `HLOCK_SECRET_…`, filled by `hlock secret set`.
//!
//! The secrets file holds references only; a literal value there is an error.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::error::{Error, Result};
use crate::profiles::Ctx;

/// The prefix every placeholder variable carries.
pub const PREFIX: &str = "HLOCK_SECRET_";
/// The keychain service hlock stores secrets under.
pub const SERVICE: &str = "hlock";
const COMMAND_TIMEOUT: Duration = Duration::from_secs(120);

/// A secret's value and where it came from.
#[derive(Debug, Clone)]
pub struct Found {
    pub value: String,
    /// `environment`, `keychain`, or `secrets file (command)` and the like.
    pub source: String,
}

/// `GITHUB_TOKEN` or `HLOCK_SECRET_GITHUB_TOKEN` → `HLOCK_SECRET_GITHUB_TOKEN`.
pub fn full_name(name: &str) -> String {
    let n = name.trim().trim_start_matches('$').trim_start_matches('{').trim_end_matches('}');
    if n.starts_with(PREFIX) {
        n.to_string()
    } else {
        format!("{PREFIX}{n}")
    }
}

/// The user's secrets file.
pub fn secrets_file(ctx: &Ctx) -> PathBuf {
    match ctx.var("HLOCK_SECRETS_FILE") {
        Some(p) => ctx.expand(p, None),
        None => ctx.expand("~/.config/hlock/secrets.toml", None),
    }
}

/// One reference in the secrets file.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Reference {
    /// Run with `sh -c`; the value is its trimmed stdout.
    Command(String),
    /// Read another environment variable.
    Env(String),
    /// Read a file (trimmed).
    File(String),
}

impl Reference {
    fn kind(&self) -> &'static str {
        match self {
            Reference::Command(_) => "command",
            Reference::Env(_) => "env",
            Reference::File(_) => "file",
        }
    }
}

/// The references in the secrets file, by full name. A missing file is empty.
pub fn references(ctx: &Ctx) -> Result<BTreeMap<String, Reference>> {
    let path = secrets_file(ctx);
    let Ok(text) = std::fs::read_to_string(&path) else { return Ok(BTreeMap::new()) };
    let bad = |m: String| Error::Profile(format!("{}: {m}", path.display()));
    let doc: toml::Value = toml::from_str(&text).map_err(|e| bad(e.to_string()))?;
    let Some(table) = doc.get("secrets") else { return Ok(BTreeMap::new()) };
    let table = table.as_table().ok_or_else(|| bad("[secrets] must be a table".into()))?;
    let mut out = BTreeMap::new();
    for (name, v) in table {
        let Some(entry) = v.as_table() else {
            return Err(bad(format!(
                "{name} is a literal value; this file holds references only. Store the value with `hlock secret set {name}`, or reference it: {name} = {{ command = \"…\" }}"
            )));
        };
        let field = |k: &str| entry.get(k).and_then(|x| x.as_str()).map(str::to_string);
        let r = match (field("command"), field("env"), field("file")) {
            (Some(c), None, None) => Reference::Command(c),
            (None, Some(e), None) => Reference::Env(e),
            (None, None, Some(f)) => Reference::File(f),
            _ => return Err(bad(format!("{name} needs exactly one of `command`, `env` or `file`"))),
        };
        out.insert(full_name(name), r);
    }
    Ok(out)
}

// Commands can be slow or prompt (`op read` asks for Touch ID), so each runs once per process.
static COMMAND_CACHE: Mutex<BTreeMap<String, Option<String>>> = Mutex::new(BTreeMap::new());

fn run_command(cmd: &str, ctx: &Ctx) -> Option<String> {
    if let Some(hit) = COMMAND_CACHE.lock().ok().and_then(|c| c.get(cmd).cloned()) {
        return hit;
    }
    let value = (|| {
        let mut child = Command::new("sh")
            .args(["-c", cmd])
            .current_dir(&ctx.cwd)
            .env_clear()
            .envs(&ctx.env)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .ok()?;
        let start = Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if start.elapsed() > COMMAND_TIMEOUT => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                Err(_) => return None,
            }
        }
        let out = child.wait_with_output().ok()?;
        let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
        (out.status.success() && !v.is_empty()).then_some(v)
    })();
    if let Ok(mut c) = COMMAND_CACHE.lock() {
        c.insert(cmd.to_string(), value.clone());
    }
    value
}

fn resolve_reference(r: &Reference, ctx: &Ctx) -> Option<String> {
    match r {
        Reference::Command(c) => run_command(c, ctx),
        Reference::Env(v) => ctx.var(v).map(str::to_string),
        Reference::File(f) => std::fs::read_to_string(ctx.expand(f, None)).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()),
    }
}

/// Find a secret's value. `name` is the placeholder variable (`HLOCK_SECRET_…`).
pub fn lookup(name: &str, ctx: &Ctx) -> Option<Found> {
    let name = full_name(name);
    if let Some(v) = ctx.var(&name) {
        return Some(Found { value: v.to_string(), source: "environment".into() });
    }
    if let Some(r) = references(ctx).ok().and_then(|m| m.get(&name).cloned()) {
        return resolve_reference(&r, ctx).map(|value| Found { value, source: format!("secrets file ({})", r.kind()) });
    }
    keychain_get(&name, ctx).map(|value| Found { value, source: "keychain".into() })
}

/// Where [`lookup`] would find a secret, without running a referenced command.
pub fn locate(name: &str, ctx: &Ctx) -> Option<String> {
    let name = full_name(name);
    if ctx.var(&name).is_some() {
        return Some("environment".into());
    }
    if let Some(r) = references(ctx).ok().and_then(|m| m.get(&name).cloned()) {
        return Some(format!("secrets file ({})", r.kind()));
    }
    keychain_get(&name, ctx).map(|_| "keychain".into())
}

// --------------------------------------------------------------------------- //
// Keychain
// --------------------------------------------------------------------------- //

/// The keychain CLI on this machine: macOS `security`, else Linux `secret-tool`.
/// `HLOCK_KEYCHAIN=0` turns the keychain off.
pub fn keychain(ctx: &Ctx) -> Option<(&'static str, PathBuf)> {
    if matches!(ctx.var("HLOCK_KEYCHAIN"), Some("0" | "false" | "off")) {
        return None;
    }
    if cfg!(target_os = "macos") {
        if let Some(p) = ctx.which("security") {
            return Some(("security", p));
        }
    }
    ctx.which("secret-tool").map(|p| ("secret-tool", p))
}

fn keychain_run(bin: &Path, args: &[&str], stdin: Option<&str>, ctx: &Ctx) -> Option<(bool, String)> {
    let mut child = Command::new(bin)
        .args(args)
        .env_clear()
        .envs(&ctx.env)
        .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    if let (Some(input), Some(mut pipe)) = (stdin, child.stdin.take()) {
        pipe.write_all(input.as_bytes()).ok()?;
    }
    let out = child.wait_with_output().ok()?;
    Some((out.status.success(), String::from_utf8_lossy(&out.stdout).into_owned()))
}

/// A secret from the keychain, if there is one.
pub fn keychain_get(name: &str, ctx: &Ctx) -> Option<String> {
    let (kind, bin) = keychain(ctx)?;
    let name = full_name(name);
    let args: Vec<&str> = match kind {
        "security" => vec!["find-generic-password", "-s", SERVICE, "-a", &name, "-w"],
        _ => vec!["lookup", "service", SERVICE, "account", &name],
    };
    let (ok, out) = keychain_run(&bin, &args, None, ctx)?;
    let v = out.trim_end_matches(['\n', '\r']).to_string();
    (ok && !v.is_empty()).then_some(v)
}

/// Store a secret in the keychain. The value goes over stdin, never on a command line.
pub fn keychain_set(name: &str, value: &str, ctx: &Ctx) -> Result<String> {
    let (kind, bin) = keychain(ctx).ok_or_else(no_keychain)?;
    let name = full_name(name);
    if value.is_empty() || value.contains(['\n', '\r']) {
        return Err(Error::Manifest("a secret must be one non-empty line".into()));
    }
    let ok = match kind {
        "security" => {
            // `security -i` reads commands from stdin; quote the value for its parser
            let quoted = value.replace('\\', "\\\\").replace('"', "\\\"");
            let line = format!("add-generic-password -U -s {SERVICE} -a {name} -l \"hlock {name}\" -w \"{quoted}\"\n");
            keychain_run(&bin, &["-i"], Some(&line), ctx)
        }
        _ => keychain_run(&bin, &["store", "--label", &format!("hlock {name}"), "service", SERVICE, "account", &name], Some(value), ctx),
    };
    match ok {
        Some((true, _)) if keychain_get(&name, ctx).as_deref() == Some(value) => Ok(name),
        _ => Err(Error::Profile(format!("couldn't store {name} with {kind}"))),
    }
}

/// Remove a secret from the keychain; false when there was none.
pub fn keychain_delete(name: &str, ctx: &Ctx) -> Result<bool> {
    let (kind, bin) = keychain(ctx).ok_or_else(no_keychain)?;
    let name = full_name(name);
    if keychain_get(&name, ctx).is_none() {
        return Ok(false);
    }
    let args: Vec<&str> = match kind {
        "security" => vec!["delete-generic-password", "-s", SERVICE, "-a", &name],
        _ => vec!["clear", "service", SERVICE, "account", &name],
    };
    keychain_run(&bin, &args, None, ctx);
    Ok(keychain_get(&name, ctx).is_none())
}

fn no_keychain() -> Error {
    Error::Profile(
        "no keychain found (macOS `security`, or Linux `secret-tool` from libsecret); \
         export the variable, or reference it in ~/.config/hlock/secrets.toml"
            .into(),
    )
}

// --------------------------------------------------------------------------- //
// Environment references
// --------------------------------------------------------------------------- //

/// A harness's own reference to an environment variable, from a profile template
/// like `${{name}}` (Claude Code) or `{env:{name}}` (OpenCode).
pub fn env_ref(template: &str, var: &str) -> String {
    template.replace("{name}", var)
}

/// Restrict a file to its owner (it holds a secret value).
pub fn restrict(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(env: &[(&str, &str)], home: &Path) -> Ctx {
        Ctx { home: home.into(), cwd: home.into(), env: env.iter().map(|(k, v)| (k.to_string(), v.to_string())).chain([("HLOCK_KEYCHAIN".into(), "0".into())]).collect(), path: vec![] }
    }

    #[test]
    fn names() {
        assert_eq!(full_name("GH_TOKEN"), "HLOCK_SECRET_GH_TOKEN");
        assert_eq!(full_name("HLOCK_SECRET_GH_TOKEN"), "HLOCK_SECRET_GH_TOKEN");
        assert_eq!(full_name("${HLOCK_SECRET_GH_TOKEN}"), "HLOCK_SECRET_GH_TOKEN");
        assert_eq!(env_ref("${{name}}", "HLOCK_SECRET_X"), "${HLOCK_SECRET_X}");
        assert_eq!(env_ref("{env:{name}}", "HLOCK_SECRET_X"), "{env:HLOCK_SECRET_X}");
    }

    #[test]
    fn sources_in_order() {
        let dir = std::env::temp_dir().join(format!("hlock-secrets-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".config/hlock")).unwrap();
        std::fs::write(dir.join("tok"), "from-file\n").unwrap();
        std::fs::write(
            dir.join(".config/hlock/secrets.toml"),
            "[secrets]\nA = { command = \"printf from-command\" }\nHLOCK_SECRET_B = { env = \"OTHER\" }\nC = { file = \"~/tok\" }\nD = { command = \"false\" }\n",
        )
        .unwrap();
        let c = ctx(&[("OTHER", "from-other"), ("HLOCK_SECRET_E", "from-env")], &dir);
        let got = |n: &str| lookup(n, &c).map(|f| (f.value, f.source));
        assert_eq!(got("A"), Some(("from-command".into(), "secrets file (command)".into())));
        assert_eq!(got("B"), Some(("from-other".into(), "secrets file (env)".into())));
        assert_eq!(got("HLOCK_SECRET_C"), Some(("from-file".into(), "secrets file (file)".into())));
        assert_eq!(got("D"), None);
        assert_eq!(got("E"), Some(("from-env".into(), "environment".into())));
        assert_eq!(got("F"), None);
        // literal values don't belong in the file
        std::fs::write(dir.join(".config/hlock/secrets.toml"), "[secrets]\nA = \"sk-literal\"\n").unwrap();
        assert!(references(&c).unwrap_err().to_string().contains("hlock secret set A"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
