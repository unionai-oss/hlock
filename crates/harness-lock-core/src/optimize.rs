//! Tune optimizers (`specs/tune-optimizers.md`): the harness bundle an optimizer edits, the
//! evaluation session it calls back into (`hlock tune eval`), and the checks the core makes
//! before an edit reaches the tune lock.
//!
//! An optimizer (a backend that runs `harness.optimize`) gets a **bundle**: the base lock's
//! instruction files and skills as files, and `bundle.json`, which says which it may edit.
//! It evaluates candidates through a **session**, which runs and grades trials of a
//! candidate on the train or dev split, inside a budget, and never on the test split. When
//! it returns, the core checks the candidate for text copied from the pack ([`leaks`]) and
//! measures it against the base lock on the held-out tasks ([`sign_test`]) before any edit
//! is written ([`apply_to_lock`]).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::{Error, Result};
use crate::hash::sha256_bytes;
use crate::lockfile::{Instructions, Lockfile};

/// The component kinds an optimizer may edit in this version.
pub const KINDS: &[&str] = &["instructions", "skill"];
const MAX_COMPONENT_BYTES: usize = 15 * 1024;
const MAX_CHANGED_LINES: usize = 120;

fn err(m: impl Into<String>) -> Error {
    Error::Tuning(m.into())
}

/// One component of a bundle: an instruction file or a skill.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Component {
    /// Its path in the bundle: `instructions/CLAUDE.md`, `skills/review`.
    pub id: String,
    pub kind: String,
    /// Its name in the lock.
    pub name: String,
    pub digest: String,
    pub editable: bool,
    /// Why it can't be edited, when it can't.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Limits {
    pub max_component_bytes: usize,
    pub max_changed_lines: usize,
}

/// `bundle.json`: what's in a bundle and what an optimizer may do with it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BundleSpec {
    pub harness: String,
    pub components: Vec<Component>,
    /// Kinds of component an optimizer may add.
    pub may_add: Vec<String>,
    pub limits: Limits,
}

impl BundleSpec {
    pub fn load(dir: &Path) -> Result<BundleSpec> {
        let text = std::fs::read_to_string(dir.join("bundle.json")).map_err(|e| err(format!("can't read {}/bundle.json: {e}", dir.display())))?;
        serde_json::from_str(&text).map_err(|e| err(format!("{}/bundle.json: {e}", dir.display())))
    }
}

fn plain_path(p: &str) -> bool {
    crate::fsutil::safe_rel(p) && !p.contains('{') && !p.starts_with('~')
}

/// Where a lock entry's content is, next to the lock: its vendored copy, else the project's
/// file or directory at the path it records.
fn located(lock_dir: &Path, vendored: Option<&str>, project: &[Option<&str>]) -> Option<PathBuf> {
    vendored
        .filter(|v| crate::fsutil::safe_rel(v))
        .map(|v| lock_dir.join(v))
        .filter(|p| p.exists())
        .or_else(|| project.iter().flatten().filter(|p| plain_path(p)).map(|p| lock_dir.join(p)).find(|p| p.exists()))
}

/// Write the base lock's instruction files and skills into `dest` as a bundle, with the
/// kinds in `edit` editable. `default_instructions` (the harness's own instruction file) is
/// always there, empty when the lock has none, so an optimizer can write a system prompt.
pub fn write_seed(base: Option<&Lockfile>, lock_dir: &Path, harness: &str, default_instructions: &str, edit: &[String], dest: &Path) -> Result<BundleSpec> {
    let _ = std::fs::remove_dir_all(dest);
    std::fs::create_dir_all(dest)?;
    let can = |kind: &str| edit.iter().any(|e| e == kind);
    let mut components = Vec::new();
    let mut instructions: Vec<&Instructions> = base.map(|l| l.instructions.iter().collect()).unwrap_or_default();
    instructions.retain(|i| plain_path(&i.name));
    let fallback = Instructions { name: default_instructions.to_string(), ..Default::default() };
    if !instructions.iter().any(|i| i.name == default_instructions) {
        instructions.push(&fallback);
    }
    for i in instructions {
        let from = located(lock_dir, i.vendored.as_deref(), &[Some(&i.name)]);
        let text = from.as_deref().map(|p| crate::fsutil::read_regular(p, 1 << 20)).transpose()?.unwrap_or_default();
        let id = format!("instructions/{}", i.name);
        let out = dest.join(&id);
        std::fs::create_dir_all(out.parent().unwrap_or(dest))?;
        std::fs::write(&out, &text)?;
        let why = if !can("instructions") {
            Some("not in [tune] edit".to_string())
        } else if i.dep.declared {
            Some("a declared dependency: change it at its source".to_string())
        } else {
            None
        };
        components.push(Component { id, kind: "instructions".into(), name: i.name.clone(), digest: sha256_bytes(&text), editable: why.is_none(), why });
    }
    for s in base.map(|l| l.skill.as_slice()).unwrap_or_default().iter().filter(|s| s.enabled) {
        let source = s.source.as_deref().and_then(|p| p.strip_prefix("./"));
        let Some(dir) = located(lock_dir, s.vendored.as_deref(), &[s.path.as_deref(), source]).filter(|d| d.join("SKILL.md").is_file()) else { continue };
        if !crate::fsutil::plain_name(&s.name) {
            continue;
        }
        let id = format!("skills/{}", s.name);
        crate::fsutil::copy_tree(&dir, &dest.join(&id))?;
        let why = if !can("skill") {
            Some("not in [tune] edit".to_string())
        } else if s.dep.declared {
            Some("a declared dependency: change it at its source".to_string())
        } else {
            None
        };
        components.push(Component { id: id.clone(), kind: "skill".into(), name: s.name.clone(), digest: crate::capture::tree_digest(&dest.join(&id)), editable: why.is_none(), why });
    }
    let spec = BundleSpec {
        harness: harness.to_string(),
        components,
        may_add: edit.iter().filter(|k| KINDS.contains(&k.as_str())).cloned().collect(),
        limits: Limits { max_component_bytes: MAX_COMPONENT_BYTES, max_changed_lines: MAX_CHANGED_LINES },
    };
    std::fs::write(dest.join("bundle.json"), serde_json::to_string_pretty(&spec)? + "\n")?;
    Ok(spec)
}

/// A candidate's identity: the digest of its instruction files and skills.
pub fn candidate_digest(dir: &Path) -> String {
    let part = |d: &str| crate::capture::tree_digest(&dir.join(d));
    sha256_bytes(format!("{}\n{}", part("instructions"), part("skills")).as_bytes())
}

/// The components in a bundle directory, by id, with their files (relative to the
/// component) and their bytes.
fn components_in(dir: &Path) -> Result<BTreeMap<String, BTreeMap<String, Vec<u8>>>> {
    let mut out = BTreeMap::new();
    let read = |p: &Path| std::fs::read(p).map_err(|e| crate::error::io_at(p, e));
    for rel in crate::capture::walk_files(&dir.join("instructions")) {
        let rel = rel.to_string_lossy().replace('\\', "/");
        let bytes = read(&dir.join("instructions").join(&rel))?;
        out.insert(format!("instructions/{rel}"), BTreeMap::from([(String::new(), bytes)]));
    }
    let skills = dir.join("skills");
    if let Ok(rd) = std::fs::read_dir(&skills) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if !e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                return Err(err(format!("skills/{name} isn't a skill directory")));
            }
            let mut files = BTreeMap::new();
            for rel in crate::capture::walk_files(&e.path()) {
                files.insert(rel.to_string_lossy().replace('\\', "/"), read(&e.path().join(&rel))?);
            }
            out.insert(format!("skills/{name}"), files);
        }
    }
    Ok(out)
}

/// A component a candidate changed or added.
#[derive(Debug, Clone)]
pub struct Change {
    pub id: String,
    pub kind: String,
    pub name: String,
    /// `modify` or `add`.
    pub op: String,
    pub before: BTreeMap<String, Vec<u8>>,
    pub after: BTreeMap<String, Vec<u8>>,
}

impl Change {
    /// The lines added and removed, over its files.
    pub fn changed_lines(&self) -> (usize, usize) {
        let names: BTreeSet<&String> = self.before.keys().chain(self.after.keys()).collect();
        let (mut added, mut removed) = (0, 0);
        for n in names {
            let text = |m: &BTreeMap<String, Vec<u8>>| m.get(n).map(|b| String::from_utf8_lossy(b).into_owned()).unwrap_or_default();
            let (a, b) = (text(&self.before), text(&self.after));
            for c in similar::TextDiff::from_lines(&a, &b).iter_all_changes() {
                match c.tag() {
                    similar::ChangeTag::Insert => added += 1,
                    similar::ChangeTag::Delete => removed += 1,
                    similar::ChangeTag::Equal => {}
                }
            }
        }
        (added, removed)
    }

    /// The text of the lines it added.
    fn added_text(&self) -> String {
        let mut out = String::new();
        for (n, after) in &self.after {
            let a = self.before.get(n).map(|b| String::from_utf8_lossy(b).into_owned()).unwrap_or_default();
            let b = String::from_utf8_lossy(after).into_owned();
            for c in similar::TextDiff::from_lines(&a, &b).iter_all_changes().filter(|c| c.tag() == similar::ChangeTag::Insert) {
                out.push_str(c.value());
            }
        }
        out
    }
}

/// What `candidate` changes from the seed bundle, or why it isn't a valid candidate: only
/// editable components changed, only kinds the bundle may add were added, nothing removed,
/// text only, within the size and changed-line limits.
pub fn validate(seed: &Path, spec: &BundleSpec, candidate: &Path) -> Result<Vec<Change>> {
    let rd = std::fs::read_dir(candidate).map_err(|e| err(format!("can't read the candidate {}: {e}", candidate.display())))?;
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !matches!(name.as_str(), "instructions" | "skills" | "bundle.json") {
            return Err(err(format!("the candidate has {name}, which isn't part of a bundle (instructions/, skills/)")));
        }
    }
    let (old, new) = (components_in(seed)?, components_in(candidate)?);
    let by_id: BTreeMap<&str, &Component> = spec.components.iter().map(|c| (c.id.as_str(), c)).collect();
    let mut changes = Vec::new();
    for id in old.keys().filter(|id| !new.contains_key(*id)) {
        return Err(err(format!("the candidate removes {id}; removing a component isn't supported")));
    }
    for (id, files) in &new {
        let before = old.get(id).cloned().unwrap_or_default();
        if old.contains_key(id) && &before == files {
            continue;
        }
        let kind = if id.starts_with("skills/") { "skill" } else { "instructions" };
        let name = id.split_once('/').map(|(_, n)| n.to_string()).unwrap_or_default();
        let op = match by_id.get(id.as_str()) {
            Some(c) if !c.editable => return Err(err(format!("{id} isn't editable ({})", c.why.as_deref().unwrap_or("bundle.json"))))?,
            Some(_) => "modify",
            None if !spec.may_add.iter().any(|k| k == kind) => return Err(err(format!("the candidate adds {id}, but the bundle may only add {:?}", spec.may_add))),
            None if kind == "skill" && !crate::fsutil::plain_name(&name) => return Err(err(format!("{id}: a skill's name must be a plain directory name"))),
            None if kind == "skill" && !files.contains_key("SKILL.md") => return Err(err(format!("{id} has no SKILL.md"))),
            None if !plain_path(&name) => return Err(err(format!("{id} isn't a plain relative path"))),
            None => "add",
        };
        let size: usize = files.values().map(Vec::len).sum();
        if size > spec.limits.max_component_bytes {
            return Err(err(format!("{id} is {size} bytes, over the bundle's limit of {}", spec.limits.max_component_bytes)));
        }
        if let Some((f, _)) = files.iter().find(|(_, b)| std::str::from_utf8(b).is_err() || b.contains(&0)) {
            return Err(err(format!("{id}: {} isn't text", if f.is_empty() { id.as_str() } else { f.as_str() })));
        }
        changes.push(Change { id: id.clone(), kind: kind.into(), name, op: op.into(), before, after: files.clone() });
    }
    let lines: usize = changes.iter().map(|c| {
        let (a, r) = c.changed_lines();
        a + r
    }).sum();
    if lines > spec.limits.max_changed_lines {
        return Err(err(format!("the candidate changes {lines} lines, over the bundle's limit of {}", spec.limits.max_changed_lines)));
    }
    Ok(changes)
}

/// The text the instruction files of a bundle give a single-completion trial as its
/// system prompt: each non-empty file, in order.
pub fn system_prompt(dir: &Path) -> String {
    let mut parts = Vec::new();
    for rel in crate::capture::walk_files(&dir.join("instructions")) {
        if let Ok(t) = std::fs::read_to_string(dir.join("instructions").join(&rel)) {
            if !t.trim().is_empty() {
                parts.push(t.trim().to_string());
            }
        }
    }
    parts.join("\n\n")
}

/// The base lock with its instruction files and skills taken from bundle `dir`, saved next
/// to it as `<dir>.trial.lock` (never inside it, where it would become part of the
/// candidate), for a code harness's trials (`HLOCK_SKILLS_DIR`, `HLOCK_INSTRUCTIONS_DIR`).
pub fn trial_lock(base: Option<&Lockfile>, dir: &Path, spec: &BundleSpec) -> Result<(Lockfile, PathBuf)> {
    let mut lock = base.cloned().unwrap_or_default();
    let have = components_in(dir)?;
    let leaf = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "bundle".into());
    lock.instructions = have
        .keys()
        .filter_map(|id| id.strip_prefix("instructions/"))
        .map(|name| Instructions { name: name.to_string(), scope: "project".into(), vendored: Some(format!("{leaf}/instructions/{name}")), digest: std::fs::read(dir.join("instructions").join(name)).ok().map(|b| sha256_bytes(&b)), ..Default::default() })
        .collect();
    let mut skills = Vec::new();
    for name in have.keys().filter_map(|id| id.strip_prefix("skills/")) {
        let mut s = lock.skill.iter().find(|s| s.name == name).cloned().unwrap_or_else(|| crate::lockfile::Skill { name: name.to_string(), origin: "tune".into(), enabled: true, ..Default::default() });
        s.vendored = Some(format!("{leaf}/skills/{name}"));
        s.digest = Some(crate::capture::tree_digest(&dir.join("skills").join(name)));
        skills.push(s);
    }
    // a skill the bundle doesn't carry (one that couldn't be read) keeps its lock entry
    lock.skill.retain(|s| !spec.components.iter().any(|c| c.kind == "skill" && c.name == s.name));
    lock.skill.extend(skills);
    let path = dir.with_file_name(format!("{leaf}.trial.lock"));
    lock.save(&path)?;
    Ok((lock, path))
}

// --------------------------------------------------------------------------- //
// The evaluation session (`hlock tune eval`)
// --------------------------------------------------------------------------- //

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Budget {
    pub rollouts: Option<u64>,
    pub usd: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Usage {
    pub rollouts: u64,
    pub usd: f64,
}

/// What `hlock tune eval --session` needs to run trials the way `tune` would, saved in
/// `<workdir>/tune/<id>/session.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Session {
    pub id: String,
    pub manifest: PathBuf,
    pub lock: Option<PathBuf>,
    /// The backend that runs the trials (`--backend`, else the manifest's).
    pub backend: Option<String>,
    pub harness: String,
    pub workdir: PathBuf,
    pub train: Vec<String>,
    pub dev: Vec<String>,
    pub trials: u32,
    pub budget: Budget,
    pub used: Usage,
}

impl Session {
    pub fn dir(workdir: &Path, id: &str) -> PathBuf {
        workdir.join("tune").join(id)
    }

    pub fn load(dir: &Path) -> Result<Session> {
        let text = std::fs::read_to_string(dir.join("session.json")).map_err(|_| err(format!("no tune session at {}", dir.display())))?;
        serde_json::from_str(&text).map_err(|e| err(format!("{}/session.json: {e}", dir.display())))
    }

    pub fn save(&self, dir: &Path) -> Result<()> {
        crate::fsutil::write_atomic(&dir.join("session.json"), serde_json::to_string_pretty(self)? + "\n")
    }

    /// Whether the budget allows another trial.
    pub fn exhausted(&self) -> Option<String> {
        if let Some(r) = self.budget.rollouts.filter(|r| self.used.rollouts >= *r) {
            return Some(format!("budget exhausted: {} of {r} rollouts", self.used.rollouts));
        }
        if let Some(u) = self.budget.usd.filter(|u| self.used.usd >= *u) {
            return Some(format!("budget exhausted: ${:.2} of ${u:.2}", self.used.usd));
        }
        None
    }
}

/// A stack running `bundle`: its instruction files reach a worker trial as the system
/// prompt, and a code harness's trial through the trial lock.
pub fn overlay(stack: &crate::search::Stack, bundle: &Path) -> crate::search::Stack {
    let mut s = stack.clone();
    s.overlay = Some(crate::search::Overlay { digest: candidate_digest(bundle), system_prompt: system_prompt(bundle) });
    s
}

/// A resolver set up as `tune` sets one up for `session`, with its tasks loaded: the search
/// split's and the holdout's.
pub fn session_resolver(session: &Session) -> Result<(crate::resolver::Resolver, crate::manifest::Manifest, Vec<crate::tasks::Task>)> {
    let manifest = crate::manifest::Manifest::load(&session.manifest)?;
    let backend = crate::worker::Backend::resolve(&manifest, session.backend.as_deref())?;
    let mut m = manifest.clone();
    m.candidates.harness = vec![crate::manifest::HarnessCandidate { name: session.harness.clone(), ..Default::default() }];
    let mut resolver = crate::resolver::Resolver::with_backend(m, &session.workdir, backend);
    resolver.harness_override = Some(session.harness.clone());
    resolver.prepare_trials()?;
    let mut tasks = resolver.load_packs();
    tasks.extend(resolver.holdout().to_vec());
    Ok((resolver, manifest, tasks))
}

/// One trial of `stack` on `task`, through the session's cache: a candidate's trial on a
/// task and seed runs once.
pub(crate) fn cached_trial(dir: &Path, resolver: &crate::resolver::Resolver, stack: &crate::search::Stack, task: &crate::tasks::Task, seed: u64) -> Result<(crate::search::TrialResult, bool)> {
    let key = crate::hash::sha256_hex(format!("{}\0{}\0{seed}", stack.hash(), task.id).as_bytes());
    let path = dir.join("cache").join(format!("{}.json", &key[..32]));
    if let Some(r) = std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str(&t).ok()) {
        return Ok((r, true));
    }
    let r = resolver.run_trial(stack, task, seed, &dir.join("runs"));
    std::fs::create_dir_all(path.parent().unwrap_or(dir))?;
    crate::fsutil::write_atomic(&path, serde_json::to_string(&r)?)?;
    Ok((r, false))
}

/// `hlock tune eval`: run `candidate` on `tasks` of `split` (`train` or `dev`), `trials`
/// times each, and grade them. Checks the candidate against the seed first, never runs a
/// test task, counts each trial that ran against the budget, and stops at it.
pub fn evaluate(dir: &Path, candidate: &Path, split: &str, tasks: Option<&[String]>, trials: Option<u32>) -> Result<Value> {
    let mut session = Session::load(dir)?;
    let allowed = match split {
        "train" => session.train.clone(),
        "dev" => session.dev.clone(),
        "test" => return Err(err("the test split is hlock's: an optimizer never sees it")),
        other => return Err(err(format!("unknown split {other:?} (train or dev)"))),
    };
    let wanted: Vec<String> = tasks.map(<[String]>::to_vec).unwrap_or_else(|| allowed.clone());
    if let Some(t) = wanted.iter().find(|t| !allowed.contains(t)) {
        return Err(err(format!("task {t:?} isn't in the {split} split")));
    }
    let seed_dir = dir.join("seed");
    let spec = BundleSpec::load(&seed_dir)?;
    validate(&seed_dir, &spec, candidate)?;
    // trials run on a snapshot, so a candidate the optimizer edits later isn't the one cached
    let digest = candidate_digest(candidate);
    let snap = dir.join("candidates").join(digest.trim_start_matches("sha256:").chars().take(12).collect::<String>());
    if !snap.join("bundle.json").is_file() {
        let _ = std::fs::remove_dir_all(&snap);
        for sub in ["instructions", "skills"] {
            if candidate.join(sub).is_dir() {
                crate::fsutil::copy_tree(&candidate.join(sub), &snap.join(sub))?;
            }
        }
        std::fs::copy(seed_dir.join("bundle.json"), snap.join("bundle.json"))?;
    }
    let (mut resolver, _, all) = session_resolver(&session)?;
    let base_lock = session.lock.as_deref().filter(|p| p.is_file()).map(Lockfile::load).transpose()?;
    let stack = overlay(&base_stack_for(&session, base_lock.as_ref())?, &snap);
    resolver.trial_locks.insert(stack.hash(), trial_lock(base_lock.as_ref(), &snap, &spec)?);
    let by_id: BTreeMap<&str, &crate::tasks::Task> = all.iter().map(|t| (t.id.as_str(), t)).collect();
    let trials = trials.unwrap_or(session.trials).max(1);
    let mut results = Vec::new();
    let mut stopped = None;
    'run: for id in &wanted {
        let task = by_id.get(id.as_str()).ok_or_else(|| err(format!("task {id:?} isn't in the eval packs any more")))?;
        for t in 0..trials {
            let key_cached = {
                let key = crate::hash::sha256_hex(format!("{}\0{}\0{t}", stack.hash(), task.id).as_bytes());
                dir.join("cache").join(format!("{}.json", &key[..32])).is_file()
            };
            if !key_cached {
                if let Some(why) = session.exhausted() {
                    stopped = Some(why);
                    break 'run;
                }
            }
            let (r, cached) = cached_trial(dir, &resolver, &stack, task, u64::from(t))?;
            if !cached {
                session.used.rollouts += 1;
                session.used.usd += r.cost_usd;
                session.save(dir)?;
            }
            let short: String = stack.hash().trim_start_matches("sha256:").chars().take(12).collect();
            results.push(json!({
                "task": task.id, "trial": t, "success": r.success, "score": r.score,
                "feedback": r.detail.clone().unwrap_or_else(|| if r.success { "passed".into() } else { "failed".into() }),
                "workspace": dir.join("runs").join(format!("{short}_{}_{t}", task.id)),
                "cost_usd": r.cost_usd, "latency_s": r.latency_s, "cached": cached,
            }));
        }
    }
    if results.is_empty() {
        if let Some(why) = stopped {
            return Err(err(why));
        }
    }
    let left = session.budget.rollouts.map(|r| r.saturating_sub(session.used.rollouts));
    Ok(json!({
        "candidate": digest,
        "split": split,
        "results": results,
        "budget": {"rollouts_used": session.used.rollouts, "rollouts_left": left, "usd_used": session.used.usd, "exhausted": stopped},
    }))
}

/// The stack a session's trials start from: the base lock's, as `tune` builds it.
fn base_stack_for(session: &Session, base: Option<&Lockfile>) -> Result<crate::search::Stack> {
    let manifest = crate::manifest::Manifest::load(&session.manifest)?;
    Ok(crate::ops::base_stack(base, &manifest, Some(&session.harness)))
}

// --------------------------------------------------------------------------- //
// Acceptance
// --------------------------------------------------------------------------- //

/// The one-sided sign test: how likely `wins` or more wins in `wins + losses` tasks that
/// differ would be if neither side were better.
pub fn sign_test(wins: usize, losses: usize) -> f64 {
    let n = wins + losses;
    if n == 0 {
        return 1.0;
    }
    let mut p = 0.0;
    let mut c = 1.0f64; // C(n, 0)
    for k in 0..=n {
        if k >= wins {
            p += c;
        }
        c = c * (n - k) as f64 / (k + 1) as f64;
    }
    p / 2f64.powi(n as i32)
}

/// Text from the eval pack that an edit mustn't copy: task ids, the literal strings graders
/// check for, and fixture file names. Short strings are left out: they'd match ordinary words.
pub fn pack_literals(tasks: &[crate::tasks::Task]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for t in tasks {
        if t.id.len() >= 4 {
            out.insert(t.id.clone());
        }
        for g in &t.graders {
            for key in ["equals", "contains"] {
                match g.get(key) {
                    Some(Value::String(s)) => {
                        out.insert(s.clone());
                    }
                    Some(Value::Array(a)) => out.extend(a.iter().filter_map(Value::as_str).map(str::to_string)),
                    _ => {}
                }
            }
        }
        if let (Some(f), Some(p)) = (&t.fixtures, &t.pack_dir) {
            for rel in crate::capture::walk_files(&p.join(f)) {
                if let Some(n) = rel.file_name() {
                    out.insert(n.to_string_lossy().into_owned());
                }
            }
        }
    }
    out.retain(|s| s.trim().len() >= 6 || (s.len() >= 4 && s.contains('_')));
    out
}

/// The pack text each change's added lines copy, by component id.
pub fn leaks(changes: &[Change], literals: &BTreeSet<String>) -> BTreeMap<String, Vec<String>> {
    let mut out = BTreeMap::new();
    for c in changes {
        let added = c.added_text();
        let hits: Vec<String> = literals.iter().filter(|l| added.contains(l.as_str())).cloned().collect();
        if !hits.is_empty() {
            out.insert(c.id.clone(), hits);
        }
    }
    out
}

/// Put `id`'s seed version back into `candidate`, dropping the optimizer's edit to it.
pub fn revert(seed: &Path, candidate: &Path, id: &str) -> Result<()> {
    let (from, to) = (seed.join(id), candidate.join(id));
    if to.is_dir() {
        std::fs::remove_dir_all(&to)?;
    } else if to.exists() {
        std::fs::remove_file(&to)?;
    }
    if from.is_dir() {
        crate::fsutil::copy_tree(&from, &to)?;
    } else if from.is_file() {
        std::fs::copy(&from, &to)?;
    }
    Ok(())
}

/// Write each change's content next to the tune lock, under `.hlock/tuned/<digest>/`, and
/// point the lock's entry for it there. Returns the `[[tune.edit]]` entries.
pub fn apply_to_lock(lf: &mut Lockfile, out_dir: &Path, candidate: &Path, changes: &[Change], ledger: &[Value]) -> Result<Vec<Value>> {
    let mut edits = Vec::new();
    for c in changes {
        let src = candidate.join(&c.id);
        let (after, before) = if c.kind == "skill" {
            (crate::capture::tree_digest(&src), lf.skill.iter().find(|s| s.name == c.name).and_then(|s| s.digest.clone()))
        } else {
            (sha256_bytes(&std::fs::read(&src)?), lf.instructions.iter().find(|i| i.name == c.name).and_then(|i| i.digest.clone()))
        };
        let short: String = after.trim_start_matches("sha256:").chars().take(8).collect();
        let rel = if c.kind == "skill" { format!(".hlock/tuned/{short}/skills/{}", c.name) } else { format!(".hlock/tuned/{short}/{}", c.name) };
        let dest = out_dir.join(&rel);
        if c.kind == "skill" {
            let _ = std::fs::remove_dir_all(&dest);
            crate::fsutil::copy_tree(&src, &dest)?;
            let scan = crate::scan::record(&crate::scan::scan_path(&dest, &rel), false);
            match lf.skill.iter_mut().find(|s| s.name == c.name) {
                Some(s) => {
                    s.digest = Some(after.clone());
                    s.vendored = Some(rel.clone());
                    s.scan = Some(scan);
                    s.scripts = Some(crate::capture::skill_scripts(&dest));
                }
                None => lf.skill.push(crate::lockfile::Skill {
                    name: c.name.clone(),
                    origin: "tune".into(),
                    digest: Some(after.clone()),
                    enabled: true,
                    scope: Some("project".into()),
                    vendored: Some(rel.clone()),
                    scan: Some(scan),
                    scripts: Some(crate::capture::skill_scripts(&dest)),
                    ..Default::default()
                }),
            }
        } else {
            std::fs::create_dir_all(dest.parent().unwrap_or(out_dir))?;
            std::fs::copy(&src, &dest)?;
            let text = std::fs::read_to_string(&dest).unwrap_or_default();
            let entry = Instructions { name: c.name.clone(), digest: Some(after.clone()), scope: "project".into(), vendored: Some(rel.clone()), source: Some("tune".into()), scan: Some(crate::scan::record(&crate::scan::scan_text(&text, &rel), false)), ..Default::default() };
            match lf.instructions.iter_mut().find(|i| i.name == c.name) {
                Some(i) => *i = entry,
                None => lf.instructions.push(entry),
            }
        }
        let note = ledger.iter().rev().find(|l| l["component"].as_str() == Some(c.id.as_str()));
        // a file the lock had no entry for is an addition to the lock, whatever the bundle held
        let op = if before.is_none() { "add" } else { c.op.as_str() };
        let mut e = json!({"component": c.id, "kind": c.kind, "op": op, "after": after, "path": rel});
        if let Some(b) = before {
            e["before"] = json!(b);
        }
        if let Some(r) = note.and_then(|l| l["rationale"].as_str()) {
            e["rationale"] = json!(r);
        }
        edits.push(e);
    }
    Ok(edits)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("hlock-optimize-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn seed(d: &Path, edit: &[&str]) -> (PathBuf, BundleSpec) {
        std::fs::write(d.join("CLAUDE.md"), "Be brief.\n").unwrap();
        std::fs::create_dir_all(d.join("skills/review")).unwrap();
        std::fs::write(d.join("skills/review/SKILL.md"), "---\nname: review\n---\nRead the diff.\n").unwrap();
        let lock = Lockfile {
            instructions: vec![Instructions { name: "CLAUDE.md".into(), scope: "project".into(), ..Default::default() }],
            skill: vec![crate::lockfile::Skill { name: "review".into(), path: Some("skills/review".into()), enabled: true, ..Default::default() }],
            ..Default::default()
        };
        let edit: Vec<String> = edit.iter().map(|s| s.to_string()).collect();
        let dest = d.join("seed");
        let spec = write_seed(Some(&lock), d, "claude-code", "CLAUDE.md", &edit, &dest).unwrap();
        (dest, spec)
    }

    fn copy(from: &Path, to: &Path) {
        crate::fsutil::copy_tree(from, to).unwrap();
    }

    #[test]
    fn the_seed_bundle_holds_the_lock_s_text_and_says_what_is_editable() {
        let d = tmp("seed");
        let (s, spec) = seed(&d, &["instructions"]);
        assert_eq!(std::fs::read_to_string(s.join("instructions/CLAUDE.md")).unwrap(), "Be brief.\n");
        assert!(s.join("skills/review/SKILL.md").is_file());
        let by: BTreeMap<&str, bool> = spec.components.iter().map(|c| (c.id.as_str(), c.editable)).collect();
        assert_eq!(by, BTreeMap::from([("instructions/CLAUDE.md", true), ("skills/review", false)]));
        assert_eq!(spec.may_add, ["instructions"]);
        assert_eq!(BundleSpec::load(&s).unwrap(), spec);
    }

    #[test]
    fn validation_allows_editable_changes_and_refuses_the_rest() {
        let d = tmp("validate");
        let (s, spec) = seed(&d, &["instructions"]);
        let cand = d.join("cand");
        copy(&s, &cand);
        assert!(validate(&s, &spec, &cand).unwrap().is_empty());
        std::fs::write(cand.join("instructions/CLAUDE.md"), "Be brief.\nRun the tests first.\n").unwrap();
        let ch = validate(&s, &spec, &cand).unwrap();
        assert_eq!((ch[0].id.as_str(), ch[0].op.as_str(), ch[0].changed_lines()), ("instructions/CLAUDE.md", "modify", (1, 0)));
        std::fs::write(cand.join("skills/review/SKILL.md"), "changed\n").unwrap();
        assert!(validate(&s, &spec, &cand).unwrap_err().to_string().contains("skills/review isn't editable"));
        std::fs::write(cand.join("skills/review/SKILL.md"), "---\nname: review\n---\nRead the diff.\n").unwrap();
        std::fs::create_dir_all(cand.join("skills/new")).unwrap();
        std::fs::write(cand.join("skills/new/SKILL.md"), "x\n").unwrap();
        assert!(validate(&s, &spec, &cand).unwrap_err().to_string().contains("may only add"));
        std::fs::remove_dir_all(cand.join("skills/new")).unwrap();
        std::fs::write(cand.join("instructions/CLAUDE.md"), "x\n".repeat(200)).unwrap();
        assert!(validate(&s, &spec, &cand).unwrap_err().to_string().contains("over the bundle's limit"));
        std::fs::write(cand.join("evil.sh"), "x").unwrap();
        assert!(validate(&s, &spec, &cand).unwrap_err().to_string().contains("isn't part of a bundle"));
    }

    #[test]
    fn the_sign_test() {
        assert!((sign_test(5, 0) - 1.0 / 32.0).abs() < 1e-12);
        assert!((sign_test(0, 0) - 1.0).abs() < 1e-12);
        assert!((sign_test(1, 1) - 0.75).abs() < 1e-12);
        assert!(sign_test(9, 1) < 0.05 && sign_test(2, 0) > 0.05);
    }

    #[test]
    fn an_edit_that_copies_pack_text_is_caught() {
        let c = Change {
            id: "instructions/CLAUDE.md".into(),
            kind: "instructions".into(),
            name: "CLAUDE.md".into(),
            op: "modify".into(),
            before: BTreeMap::from([(String::new(), b"For count_words, be careful.\n".to_vec())]),
            after: BTreeMap::from([(String::new(), b"For count_words, be careful.\nThe answer is 42 widgets.\n".to_vec())]),
        };
        let lits = BTreeSet::from(["count_words".to_string(), "42 widgets".to_string()]);
        // only what the edit added counts: the task id was already there
        assert_eq!(leaks(&[c], &lits), BTreeMap::from([("instructions/CLAUDE.md".to_string(), vec!["42 widgets".to_string()])]));
    }

    #[test]
    fn applying_writes_tuned_files_and_points_the_lock_at_them() {
        let d = tmp("apply");
        let (s, spec) = seed(&d, &["instructions", "skill"]);
        let cand = d.join("cand");
        copy(&s, &cand);
        std::fs::write(cand.join("instructions/CLAUDE.md"), "Be brief.\nRun the tests first.\n").unwrap();
        let changes = validate(&s, &spec, &cand).unwrap();
        let mut lf = Lockfile { instructions: vec![Instructions { name: "CLAUDE.md".into(), digest: Some(sha256_bytes(b"Be brief.\n")), ..Default::default() }], ..Default::default() };
        let ledger = vec![json!({"component": "instructions/CLAUDE.md", "op": "modify", "rationale": "tests were skipped"})];
        let edits = apply_to_lock(&mut lf, &d, &cand, &changes, &ledger).unwrap();
        let i = &lf.instructions[0];
        assert_eq!(i.source.as_deref(), Some("tune"));
        let rel = i.vendored.clone().unwrap();
        assert!(rel.starts_with(".hlock/tuned/") && rel.ends_with("/CLAUDE.md"));
        assert_eq!(std::fs::read_to_string(d.join(&rel)).unwrap(), "Be brief.\nRun the tests first.\n");
        assert_eq!(edits[0]["rationale"], "tests were skipped");
        assert_eq!(edits[0]["before"], json!(sha256_bytes(b"Be brief.\n")));
    }

    #[test]
    fn the_trial_lock_places_the_bundle() {
        let d = tmp("trial");
        let (s, spec) = seed(&d, &["instructions"]);
        let (lock, path) = trial_lock(None, &s, &spec).unwrap();
        assert_eq!(path, d.join("seed.trial.lock"));
        assert_eq!(lock.instructions[0].vendored.as_deref(), Some("seed/instructions/CLAUDE.md"));
        assert_eq!(lock.skill[0].vendored.as_deref(), Some("seed/skills/review"));
        assert_eq!(system_prompt(&s), "Be brief.");
    }
}
