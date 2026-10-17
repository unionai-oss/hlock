//! `[target] harness` / `--harness`: narrowing harness candidates to the target.

use harness_lock_core::resolver::apply_target;
use harness_lock_core::{Manifest, Registry};

fn manifest(extra: &str) -> Manifest {
    Manifest::from_str(&format!("[project]\nname = \"t\"\n{extra}")).unwrap()
}

fn names(m: &Manifest) -> Vec<String> {
    m.candidates.harness.iter().map(|h| h.name.clone()).collect()
}

#[test]
fn no_target_is_a_noop() {
    let r = Registry::builtin();
    let mut m = manifest("[candidates]\nharness = [\"reference\"]\n");
    assert_eq!(apply_target(&mut m, None, &r).unwrap(), None);
    assert_eq!(names(&m), ["reference"]);
}

#[test]
fn manifest_target_replaces_default_reference() {
    let r = Registry::builtin();
    let mut m = manifest("[candidates]\nharness = [\"reference\"]\n[target]\nharness = \"claude\"\n");
    assert_eq!(m.target_harness().as_deref(), Some("claude"));
    assert_eq!(apply_target(&mut m, None, &r).unwrap().as_deref(), Some("claude-code"));
    assert_eq!(names(&m), ["claude-code"]);
}

#[test]
fn cli_override_wins_and_narrows_candidates() {
    let r = Registry::builtin();
    let mut m = manifest("[candidates]\nharness = [\"opencode\", \"pi\"]\n[target]\nharness = \"opencode\"\n");
    assert_eq!(apply_target(&mut m, Some("pi"), &r).unwrap().as_deref(), Some("pi"));
    assert_eq!(names(&m), ["pi"]);
}

#[test]
fn target_outside_candidates_is_an_error() {
    let r = Registry::builtin();
    let mut m = manifest("[candidates]\nharness = [\"opencode\"]\n");
    let err = apply_target(&mut m, Some("codex"), &r).unwrap_err().to_string();
    assert!(err.contains("not one of [candidates] harness"), "{err}");
    let err = apply_target(&mut m, Some("not-a-harness"), &r).unwrap_err().to_string();
    assert!(err.contains("unknown harness"), "{err}");
}
