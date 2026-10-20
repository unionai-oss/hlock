//! Declared dependencies end to end, against local git repositories standing in for
//! GitHub (`HLOCK_GITHUB_URL`): `add`, the lock, placement, `check`, `refresh`, APM
//! packages resolved as one graph, and `apm.lock.yaml` export.
//!
//! One process (this file) owns the environment variables it sets.

use std::path::{Path, PathBuf};
use std::process::Command;

use harness_lock_core::deps::{self, AddRequest, Kind};
use harness_lock_core::profiles::{Ctx, Registry};
use harness_lock_core::{Lockfile, Manifest};

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "a")
        .env("GIT_AUTHOR_EMAIL", "a@x")
        .env("GIT_COMMITTER_NAME", "a")
        .env("GIT_COMMITTER_EMAIL", "a@x")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn write(p: &Path, text: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

struct World {
    root: PathBuf,
    remotes: PathBuf,
}

impl World {
    /// One remotes directory and `HLOCK_HOME` per process (tests run in parallel); each
    /// test gets its own project and repository names.
    fn new(tag: &str) -> World {
        let base = std::env::temp_dir().join(format!("hlock-deps-it-{}", std::process::id()));
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            let _ = std::fs::remove_dir_all(&base);
            std::fs::create_dir_all(base.join("remotes")).unwrap();
            std::env::set_var("HLOCK_GITHUB_URL", format!("file://{}", base.join("remotes").display()));
            std::env::set_var("HLOCK_HOME", base.join("hlock-home"));
        });
        let root = base.join(tag);
        std::fs::create_dir_all(&root).unwrap();
        World { root, remotes: base.join("remotes") }
    }

    /// A repository `owner/name` with `files`, committed and tagged with each of `tags` in turn.
    fn repo(&self, owner_repo: &str, versions: &[(&str, &[(&str, &str)])]) -> PathBuf {
        let r = self.remotes.join(format!("{owner_repo}.git"));
        std::fs::create_dir_all(&r).unwrap();
        git(&r, &["init", "-q", "-b", "main"]);
        for (tag, files) in versions {
            for (p, text) in *files {
                write(&r.join(p), text);
            }
            git(&r, &["add", "-A"]);
            git(&r, &["commit", "-q", "-m", tag]);
            if !tag.is_empty() {
                git(&r, &["tag", tag]);
            }
        }
        r
    }

    fn project(&self) -> (PathBuf, Ctx) {
        let p = self.root.join("project");
        std::fs::create_dir_all(&p).unwrap();
        let home = self.root.join("home");
        std::fs::create_dir_all(&home).unwrap();
        let mut ctx = Ctx::at(&p);
        ctx.home = home;
        ctx.env.remove("CLAUDE_CONFIG_DIR");
        ctx.env.insert("HOME".into(), ctx.home.to_string_lossy().into_owned());
        (p, ctx)
    }
}

fn skill(name: &str, body: &str) -> String {
    format!("---\nname: {name}\ndescription: d\n---\n{body}\n")
}

fn add(kind: &str, source: &str, project: &Path, reg: &Registry, ctx: &Ctx) -> deps::AddOutcome {
    let req = AddRequest { kind: Some(kind.into()), source: source.into(), harness: Some("claude-code".into()), ..Default::default() };
    let p = deps::prepare(&req, &project.join("harness.toml"), &project.join("harness.lock"), reg, ctx).unwrap();
    deps::commit(&p, reg, ctx, true, None).unwrap()
}

#[test]
fn add_check_sync_and_remove_a_skill() {
    let w = World::new("skill");
    let s1 = skill("sql", "v1.2");
    let s2 = skill("sql", "v1.3");
    w.repo("acme/skills", &[("v1.2.0", &[("skills/sql/SKILL.md", &s1)]), ("v1.3.0", &[("skills/sql/SKILL.md", &s2)]), ("v2.0.0", &[("skills/sql/SKILL.md", "---\nname: sql\ndescription: d\n---\nv2\n")])]);
    let (project, ctx) = w.project();
    let reg = Registry::builtin();

    let o = add("skill", "github:acme/skills/skills/sql@^1.2", &project, &reg, &ctx);
    assert!(o.created_manifest && o.created_lock);
    let installed = project.join(".claude/skills/sql/SKILL.md");
    assert_eq!(std::fs::read_to_string(&installed).unwrap(), s2, "the highest tag ^1.2 allows");
    let manifest = std::fs::read_to_string(project.join("harness.toml")).unwrap();
    assert!(manifest.contains("[skills]\nsql = \"github:acme/skills/skills/sql@^1.2\""), "{manifest}");
    let lock = Lockfile::load(&project.join("harness.lock")).unwrap();
    let s = lock.skill.iter().find(|s| s.name == "sql").unwrap();
    assert!(s.dep.declared && s.vendored.is_none());
    assert_eq!((s.dep.constraint.as_deref(), s.dep.resolved_tag.as_deref(), s.path.as_deref()), (Some("^1.2"), Some("v1.3.0"), Some(".claude/skills/sql")));
    assert_eq!(lock.manifest_hash, Some(Manifest::load(&project.join("harness.toml")).unwrap().deps_hash()));

    let m = Manifest::load(&project.join("harness.toml")).unwrap();
    assert_eq!(deps::check(Some(&m), &lock, &reg, &ctx).unwrap(), vec![], "clean after add");
    // a pin of the project afterwards stays clean too
    assert!(harness_lock_core::capture::check_pin(&lock, &reg, &ctx).unwrap().is_empty());

    // declared, changed
    write(&installed, "edited");
    let issues = deps::check(Some(&m), &lock, &reg, &ctx).unwrap();
    assert_eq!(issues.iter().map(|i| i.problem.as_str()).collect::<Vec<_>>(), ["changed"]);
    // declared, missing; sync's plan fetches it at its commit, from the store
    std::fs::remove_dir_all(project.join(".claude/skills/sql")).unwrap();
    assert_eq!(deps::check(Some(&m), &lock, &reg, &ctx).unwrap()[0].problem, "missing");
    let plan = harness_lock_core::sync::plan(&lock, &project.join("harness.lock"), &reg, &ctx, &harness_lock_core::sync::SyncOptions::default()).unwrap();
    let step = plan.steps.iter().find(|s| s.id == "skill:sql").unwrap();
    assert_eq!(step.status, "auto", "{step:?}");
    harness_lock_core::sync::run_step(step, &ctx).unwrap();
    assert_eq!(std::fs::read_to_string(&installed).unwrap(), s2);
    // even with the store gone, the git cache has the commit
    std::fs::remove_dir_all(project.join(".hlock/store")).unwrap();
    std::fs::remove_dir_all(project.join(".claude/skills/sql")).unwrap();
    harness_lock_core::sync::run_step(step, &ctx).unwrap();
    assert_eq!(std::fs::read_to_string(&installed).unwrap(), s2);

    // a changed declaration: refresh resolves it again (no network for what's unchanged)
    let mut lock2 = lock.clone();
    assert!(deps::refresh(Some(&m), &mut lock2, &reg, &ctx, false).unwrap().is_empty());
    std::fs::write(project.join("harness.toml"), manifest.replace("@^1.2", "@^2")).unwrap();
    let m2 = Manifest::load(&project.join("harness.toml")).unwrap();
    assert_eq!(deps::check(Some(&m2), &lock2, &reg, &ctx).unwrap()[0].problem, "unlocked");
    assert!(deps::refresh(Some(&m2), &mut lock2.clone(), &reg, &ctx, true).is_err(), "--frozen refuses to change the lock");
    let changes = deps::refresh(Some(&m2), &mut lock2, &reg, &ctx, false).unwrap();
    assert_eq!(changes.len(), 1, "{changes:?}");
    assert_eq!(lock2.skill.iter().find(|s| s.name == "sql").unwrap().dep.resolved_tag.as_deref(), Some("v2.0.0"));
    std::fs::write(project.join("harness.toml"), &manifest).unwrap();

    // outdated: ^1.2 is current, v2.0.0 is newer
    let out = deps::outdated(Some(&m), &lock, &reg, &ctx, None).unwrap();
    assert_eq!((out[0].wanted.as_deref(), out[0].latest.as_deref()), (None, Some("v2.0.0")));

    let r = deps::remove(Kind::Skill, "sql", &project.join("harness.toml"), &project.join("harness.lock"), &reg, &ctx, true).unwrap();
    assert!(r.from_manifest && r.from_lock == 1 && !installed.exists(), "{r:?}");
    assert!(!std::fs::read_to_string(project.join("harness.toml")).unwrap().contains("sql ="));
    let _ = std::fs::remove_dir_all(&w.root);
}

#[test]
fn apm_packages_resolve_as_one_graph() {
    let w = World::new("apm");
    let b1 = skill("base-skill", "base 1.0");
    let b11 = skill("base-skill", "base 1.1");
    w.repo("acme/base", &[("v1.0.0", &[("apm.yml", "name: base\nversion: 1.0.0\n"), ("skills/base-skill/SKILL.md", &b1)]), ("v1.1.0", &[("apm.yml", "name: base\nversion: 1.1.0\n"), ("skills/base-skill/SKILL.md", &b11)]), ("v2.0.0", &[("apm.yml", "name: base\nversion: 2.0.0\n")])]);
    w.repo("acme/lib", &[("v1.0.0", &[("apm.yml", "name: lib\nversion: 1.0.0\nlicense: MIT\ndependencies:\n  apm:\n    - acme/base#^1.0.0\n  mcp:\n    - name: helper\n      registry: false\n      transport: stdio\n      command: helper-server\n"), (".apm/instructions/style.instructions.md", "be terse\n"), (".apm/prompts/x.prompt.md", "p")])]);
    w.repo("acme/app", &[("v1.0.0", &[("apm.yml", "name: app\nversion: 1.0.0\ndependencies:\n  apm:\n    - acme/lib#^1.0.0\n    - acme/base#~1.0.0\n"), ("SKILL.md", &skill("app", "app"))])]);
    let (project, ctx) = w.project();
    let reg = Registry::builtin();

    let o = add("package", "apm:acme/app#^1.0.0", &project, &reg, &ctx);
    let names: Vec<(&str, u32)> = o.packages.iter().map(|p| (p.name.as_str(), p.depth)).collect();
    assert_eq!(names, [("app", 1), ("lib", 2), ("base", 2)], "breadth-first");
    let base = o.packages.iter().find(|p| p.name == "base").unwrap();
    assert_eq!(base.dep.resolved_tag.as_deref(), Some("v1.0.0"), "^1.0.0 from lib and ~1.0.0 from app intersect at 1.0.x");
    assert_eq!(std::fs::read_to_string(project.join(".claude/skills/base-skill/SKILL.md")).unwrap(), b1);
    assert!(project.join(".claude/skills/app/SKILL.md").is_file(), "a package with a root SKILL.md is a skill");
    assert_eq!(std::fs::read_to_string(project.join(".claude/rules/style.md")).unwrap(), "be terse\n");
    // a server a package brings is recorded, not configured: it would start a process
    assert!(!project.join(".mcp.json").exists() || !std::fs::read_to_string(project.join(".mcp.json")).unwrap().contains("helper"));
    assert!(o.warnings.iter().any(|w| w.contains("helper") && w.contains("lib brings")), "{:?}", o.warnings);
    let lib = o.packages.iter().find(|p| p.name == "lib").unwrap();
    assert_eq!((lib.license.as_deref(), lib.skipped.get("prompts")), (Some("MIT"), Some(&1)));

    let lock = Lockfile::load(&project.join("harness.lock")).unwrap();
    assert_eq!(lock.provenance.openapm.as_deref(), Some("0.1"));
    let bs = lock.skill.iter().find(|s| s.name == "base-skill").unwrap();
    assert_eq!(bs.dep.required_by, ["app", "base"]);
    let m = Manifest::load(&project.join("harness.toml")).unwrap();
    assert_eq!(deps::check(Some(&m), &lock, &reg, &ctx).unwrap(), vec![]);

    // export: what APM would write for the claude target
    let profile = reg.require_harness("claude-code").unwrap();
    let env = deps::Env::new(&ctx, profile);
    let e = harness_lock_core::apm::export(&lock, &env, Some(&project.join("apm.lock.yaml")), false).unwrap();
    assert_eq!((e.packages, e.lockfile_version.as_str()), (3, "2"));
    let back = harness_lock_core::apm::parse_lock(&e.text).unwrap();
    let app = back.iter().find(|d| d.repo_url == "acme/app").unwrap();
    assert_eq!((app.host.as_deref(), app.depth, app.constraint.as_deref()), (Some("github.com"), None, Some("^1.0.0")));
    let b = back.iter().find(|d| d.repo_url == "acme/base").unwrap();
    assert_eq!(b.depth, Some(2));
    assert!(b.deployed_files.contains(&".claude/skills/base-skill/SKILL.md".to_string()), "{:?}", b.deployed_files);
    assert!(e.text.starts_with(harness_lock_core::apm::EXPORT_HEADER) && e.text.contains("mcp_servers:\n- helper"), "{}", e.text);
    assert!(harness_lock_core::apm::stale_export(&lock, &project).is_none());

    // a third package that needs base ^2: no version satisfies both chains
    w.repo("acme/other", &[("v1.0.0", &[("apm.yml", "name: other\nversion: 1.0.0\ndependencies:\n  apm:\n    - acme/base#^2.0.0\n")])]);
    let req = AddRequest { kind: None, source: "apm:acme/other".into(), ..Default::default() };
    let e = deps::prepare(&req, &project.join("harness.toml"), &project.join("harness.lock"), &reg, &ctx).unwrap_err().to_string();
    assert!(e.contains("no version satisfies") && e.contains("app -> lib -> base") && e.contains("other -> base"), "{e}");
    let _ = std::fs::remove_dir_all(&w.root);
}
