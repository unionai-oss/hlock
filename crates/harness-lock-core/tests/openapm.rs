//! OpenAPM v0.1 conformance: the fixtures in `tests/openapm/` (copied from microsoft/apm;
//! see the README there) against hlock's dependency resolver and APM file readers.

use std::path::{Path, PathBuf};

use harness_lock_core::deps::{tag_versions, VersionRange};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("openapm")
}

#[test]
fn semver_dialect_picks_the_expected_tag() {
    let text = std::fs::read_to_string(fixtures().join("resolution/semver-dialect.json")).unwrap();
    let doc: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(doc["dialect"], "node-semver");
    let cases = doc["cases"].as_array().unwrap();
    assert!(cases.len() >= 14);
    for case in cases {
        let id = case["id"].as_str().unwrap();
        let tags: Vec<String> = case["tags"].as_array().unwrap().iter().map(|t| t.as_str().unwrap().to_string()).collect();
        let range = VersionRange::parse(case["range"].as_str().unwrap()).unwrap_or_else(|e| panic!("{id}: {e}"));
        let versions = tag_versions(&tags, "example");
        let got = range
            .best_with(versions.iter().map(|(v, t)| (v.clone(), t.as_str())), case["prerelease_optin"].as_bool().unwrap_or(false))
            .map(|(_, t)| t.to_string());
        assert_eq!(got.as_deref(), case["expected"].as_str(), "{id}: {} over {tags:?}", case["range"]);
    }
}

#[test]
fn lockfile_fixtures_read() {
    use harness_lock_core::apm::parse_lock;
    let dir = fixtures().join("lockfile");
    let mut n = 0;
    for e in std::fs::read_dir(&dir).unwrap() {
        let p = e.unwrap().path();
        let entries = parse_lock(&std::fs::read_to_string(&p).unwrap()).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
        assert!(!entries.is_empty(), "{}", p.display());
        n += 1;
    }
    assert!(n >= 4);
    let v1 = parse_lock(&std::fs::read_to_string(dir.join("v1-git-only.yml")).unwrap()).unwrap();
    assert_eq!(v1[0].resolved_commit.as_deref(), Some("7f3c9a4d2e1b8c7f0a9e6d5c4b3a2918f7e6d5c4"));
    assert_eq!(v1[0].identity(), "github.com/contoso/example");
    let v2 = parse_lock(&std::fs::read_to_string(dir.join("v2-with-registry.yml")).unwrap()).unwrap();
    let semver = v2.iter().find(|e| e.constraint.is_some()).unwrap();
    assert_eq!((semver.constraint.as_deref(), semver.resolved_tag.as_deref()), (Some("^1.0.0"), Some("v1.4.2")));
    // an unknown lockfile version is refused
    assert!(parse_lock("lockfile_version: \"3\"\ndependencies: []\n").is_err());
}

#[test]
fn manifest_fixtures_are_read_or_refused() {
    use harness_lock_core::apm::parse_manifest;
    let dir = fixtures().join("manifest");
    for e in std::fs::read_dir(&dir).unwrap() {
        let p = e.unwrap().path();
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        let r = parse_manifest(&std::fs::read_to_string(&p).unwrap());
        if name.starts_with("invalid-") {
            assert!(r.is_err(), "{name} should be refused");
        } else {
            r.unwrap_or_else(|e| panic!("{name}: {e}"));
        }
    }
}
