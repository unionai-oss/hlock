//! SBOM export (`specs/audit.md` §7): a lock as CycloneDX 1.6 or SPDX 2.3 JSON. Beyond
//! packages, commits and hashes, it has what an AI-BOM review asks for: the harness and its
//! install source, inference runtimes, and each model (CycloneDX `machine-learning-model`)
//! with its provider, revision and digest where the lock has them.

use serde_json::{json, Value};

use crate::lockfile::Lockfile;

/// One thing in the bill of materials.
#[derive(Debug, Clone)]
struct Item {
    /// A stable id (`skill:sql`).
    id: String,
    /// CycloneDX type: `application`, `library`, `file`, `machine-learning-model`.
    ty: &'static str,
    name: String,
    version: Option<String>,
    purl: Option<String>,
    /// `sha256:HEX` digests.
    digest: Option<String>,
    license: Option<String>,
    source: Option<String>,
    properties: Vec<(String, String)>,
}

/// A package URL for a source, where one exists (`pkg:npm/…`, `pkg:pypi/…`, `pkg:github/…`).
fn purl(source: &str, version: Option<&str>, commit: Option<&str>) -> Option<String> {
    let at = |v: Option<&str>| v.map(|v| format!("@{v}")).unwrap_or_default();
    let (method, rest) = source.split_once(':')?;
    let pkg = |r: &str| -> String {
        let (scope, body) = r.strip_prefix('@').map(|b| ("@", b)).unwrap_or(("", r));
        format!("{scope}{}", body.split('@').next().unwrap_or(body)).split("==").next().unwrap_or("").to_string()
    };
    match method {
        "npm" => Some(format!("pkg:npm/{}{}", pkg(rest).replace('@', "%40"), at(version))),
        "pypi" => Some(format!("pkg:pypi/{}{}", pkg(rest).to_lowercase(), at(version))),
        // Homebrew has no registered purl type: its source is a property
        "brew" | "brew-cask" => None,
        _ => {
            let subject = crate::policy::subject(source)?;
            let gh = subject.strip_prefix("github:")?;
            let mut parts = gh.splitn(3, '/');
            // GitHub names aren't case-sensitive: the purl type says they're lowercased
            let (owner, repo) = (parts.next()?.to_lowercase(), parts.next()?.to_lowercase());
            let sub = parts.next().map(|s| format!("#{s}")).unwrap_or_default();
            Some(format!("pkg:github/{owner}/{repo}{}{sub}", at(commit.or(version))))
        }
    }
}

/// Whether `l` reads as an SPDX license expression (`MIT`, `Apache-2.0 OR MIT`), not a name.
fn spdx_expression(l: &str) -> bool {
    !l.trim().is_empty() && l.split_whitespace().all(|t| matches!(t, "AND" | "OR" | "WITH") || t.trim_matches(['(', ')']).chars().all(|c| c.is_ascii_alphanumeric() || ".-+:".contains(c)) && !t.trim_matches(['(', ')']).is_empty())
}

/// Items with their ids made unique (`skill:x`, then `skill:x#2`): two entries can share a name.
fn items(lock: &Lockfile) -> Vec<Item> {
    let mut out = raw_items(lock);
    let mut seen = std::collections::BTreeMap::<String, usize>::new();
    for i in out.iter_mut() {
        let n = seen.entry(i.id.clone()).or_default();
        *n += 1;
        if *n > 1 {
            i.id = format!("{}#{n}", i.id);
        }
    }
    out
}

fn raw_items(lock: &Lockfile) -> Vec<Item> {
    let mut out = Vec::new();
    let h = &lock.harness;
    if !h.name.is_empty() {
        out.push(Item {
            id: format!("harness:{}", h.name),
            ty: "application",
            name: h.name.clone(),
            version: h.version.clone(),
            purl: h.source.as_deref().and_then(|s| purl(s, h.version.as_deref(), None)),
            digest: h.integrity.clone(),
            license: None,
            source: h.source.clone(),
            properties: vec![("hlock:kind".into(), "harness".into())],
        });
    }
    for l in &lock.llm {
        let mut props = vec![("hlock:kind".into(), "model".into()), ("hlock:role".into(), l.role.clone()), ("hlock:provider".into(), l.provider.clone())];
        if let Some(e) = &l.endpoint {
            props.push(("hlock:endpoint".into(), e.clone()));
        }
        if let Some(r) = &l.runtime {
            props.push(("hlock:runtime".into(), r.clone()));
        }
        out.push(Item { id: format!("model:{}:{}:{}", l.role, l.provider, l.model), ty: "machine-learning-model", name: l.model.clone(), version: None, purl: None, digest: None, license: None, source: Some(format!("{}:{}", l.provider, l.model)), properties: props });
    }
    for d in &lock.decider {
        let props = vec![("hlock:kind".into(), "decision-model".into()), ("hlock:use".into(), d.use_.clone()), ("hlock:provider".into(), d.provider.clone())];
        out.push(Item { id: format!("decider:{}", d.use_), ty: "machine-learning-model", name: d.model.clone(), version: None, purl: None, digest: None, license: None, source: Some(format!("{}:{}", d.provider, d.model)), properties: props });
    }
    for r in &lock.runtime {
        out.push(Item {
            id: format!("runtime:{}", r.name),
            ty: "application",
            name: r.name.clone(),
            version: r.version.clone(),
            purl: r.source.as_deref().and_then(|s| purl(s, r.version.as_deref(), None)),
            digest: None,
            license: None,
            source: r.source.clone(),
            properties: vec![("hlock:kind".into(), "runtime".into()), ("hlock:location".into(), r.location.clone())],
        });
        for m in &r.model {
            let mut props = vec![("hlock:kind".into(), "model".into()), ("hlock:runtime".into(), r.name.clone())];
            if let Some(sz) = m.size_bytes {
                props.push(("hlock:size-bytes".into(), sz.to_string()));
            }
            out.push(Item { id: format!("model:{}/{}", r.name, m.id), ty: "machine-learning-model", name: m.id.clone(), version: m.revision.clone(), purl: m.source.as_deref().and_then(|s| s.strip_prefix("hf://")).map(|repo| format!("pkg:huggingface/{repo}{}", m.revision.as_deref().map(|v| format!("@{v}")).unwrap_or_default())), digest: m.digest.clone(), license: None, source: m.source.clone(), properties: props });
        }
    }
    for t in lock.tool.iter().filter(|t| t.kind == "mcp") {
        let mut props = vec![("hlock:kind".into(), "mcp-server".into())];
        if let Some(tr) = &t.transport {
            props.push(("hlock:transport".into(), tr.clone()));
        }
        if let Some(u) = &t.url {
            props.push(("hlock:url".into(), u.clone()));
        }
        out.push(Item { id: format!("mcp:{}", t.name), ty: "application", name: t.name.clone(), version: t.version.clone(), purl: t.source.as_deref().and_then(|s| purl(s, t.version.as_deref(), None)), digest: t.integrity.clone().filter(|i| i.starts_with("sha256:")), license: None, source: t.source.clone(), properties: props });
    }
    for s in lock.skill.iter().filter(|s| s.origin != "harness-builtin") {
        let mut props = vec![("hlock:kind".into(), "skill".into()), ("hlock:origin".into(), s.origin.clone())];
        if let Some(sc) = &s.scan {
            props.push(("hlock:scan".into(), sc.clone()));
        }
        out.push(Item { id: format!("skill:{}", s.name), ty: "file", name: s.name.clone(), version: s.dep.resolved_tag.clone(), purl: s.source.as_deref().and_then(|src| purl(src, None, s.dep.commit.as_deref())), digest: s.digest.clone(), license: None, source: s.source.clone(), properties: props });
    }
    for i in &lock.instructions {
        out.push(Item { id: format!("instructions:{}", i.name), ty: "file", name: i.name.clone(), version: None, purl: i.source.as_deref().and_then(|src| purl(src, None, i.dep.commit.as_deref())), digest: i.digest.clone(), license: None, source: i.source.clone(), properties: vec![("hlock:kind".into(), "instructions".into())] });
    }
    for n in lock.native.iter().filter(|n| !n.kind.ends_with(".marketplace")) {
        out.push(Item { id: format!("plugin:{}", n.name), ty: "application", name: n.name.clone(), version: n.version.clone(), purl: n.source.as_deref().and_then(|s| purl(s, n.version.as_deref(), n.revision.as_deref())), digest: n.integrity.clone().filter(|i| i.starts_with("sha256:")), license: None, source: n.source.clone(), properties: vec![("hlock:kind".into(), n.kind.clone())] });
    }
    for p in &lock.package {
        out.push(Item { id: format!("package:{}", p.name), ty: "library", name: p.name.clone(), version: p.version.clone().or(p.dep.resolved_tag.clone()), purl: purl(&p.source, None, p.dep.commit.as_deref()), digest: p.digest.clone(), license: p.license.clone(), source: Some(p.source.clone()), properties: vec![("hlock:kind".into(), "apm-package".into()), ("hlock:depth".into(), p.depth.to_string())] });
    }
    out
}

fn hex(digest: &str) -> Option<&str> {
    digest.strip_prefix("sha256:").filter(|h| h.len() == 64 && h.chars().all(|c| c.is_ascii_hexdigit()))
}

/// A UUID (v4 layout) derived from the lock, so the same lock gives the same serial number.
fn uuid_of(lock: &Lockfile) -> String {
    let h = crate::hash::sha256_hex(serde_json::to_string(lock).unwrap_or_default().as_bytes());
    format!("{}-{}-4{}-a{}-{}", &h[..8], &h[8..12], &h[13..16], &h[17..20], &h[20..32])
}

fn when(lock: &Lockfile) -> String {
    if lock.generated_at.is_empty() { crate::pull::now_rfc3339() } else { lock.generated_at.clone() }
}

fn stack_name(lock: &Lockfile) -> String {
    lock.stack.get("name").and_then(Value::as_str).unwrap_or("agent").to_string()
}

/// The lock as a CycloneDX 1.6 JSON BOM.
pub fn cyclonedx(lock: &Lockfile) -> Value {
    let items = items(lock);
    let components: Vec<Value> = items
        .iter()
        .map(|i| {
            let mut c = json!({"type": i.ty, "bom-ref": i.id, "name": i.name});
            if let Some(v) = &i.version {
                c["version"] = json!(v);
            }
            if let Some(p) = &i.purl {
                c["purl"] = json!(p);
            }
            if let Some(h) = i.digest.as_deref().and_then(hex) {
                c["hashes"] = json!([{"alg": "SHA-256", "content": h}]);
            }
            match &i.license {
                Some(l) if spdx_expression(l) => c["licenses"] = json!([{"expression": l}]),
                Some(l) => c["licenses"] = json!([{"license": {"name": l}}]),
                None => {}
            }
            let mut props = i.properties.clone();
            match &i.source {
                // an external reference is a URL; any other source is a property
                Some(s) if s.contains("://") => c["externalReferences"] = json!([{"type": "distribution", "url": s.trim_start_matches("git+")}]),
                Some(s) => props.push(("hlock:source".into(), s.clone())),
                None => {}
            }
            if !props.is_empty() {
                c["properties"] = json!(props.iter().map(|(k, v)| json!({"name": k, "value": v})).collect::<Vec<_>>());
            }
            c
        })
        .collect();
    json!({
        "bomFormat": "CycloneDX",
        "specVersion": "1.6",
        "serialNumber": format!("urn:uuid:{}", uuid_of(lock)),
        "version": 1,
        "metadata": {
            "timestamp": when(lock),
            "tools": {"components": [{"type": "application", "name": "hlock", "version": env!("CARGO_PKG_VERSION")}]},
            "component": {"type": "application", "bom-ref": "stack", "name": stack_name(lock)},
        },
        "components": components,
        "dependencies": [{"ref": "stack", "dependsOn": items.iter().map(|i| i.id.clone()).collect::<Vec<_>>()}],
    })
}

/// An SPDX id from an item id (`skill:sql` → `SPDXRef-skill-sql`).
fn spdx_id(id: &str) -> String {
    let s: String = id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '-' }).collect();
    format!("SPDXRef-{s}")
}

/// The lock as an SPDX 2.3 JSON document.
pub fn spdx(lock: &Lockfile) -> Value {
    let items = items(lock);
    let mut packages = vec![json!({"name": stack_name(lock), "SPDXID": "SPDXRef-stack", "downloadLocation": "NOASSERTION", "filesAnalyzed": false, "primaryPackagePurpose": "APPLICATION", "licenseConcluded": "NOASSERTION", "copyrightText": "NOASSERTION"})];
    let mut relationships = vec![json!({"spdxElementId": "SPDXRef-DOCUMENT", "relationshipType": "DESCRIBES", "relatedSpdxElement": "SPDXRef-stack"})];
    // ids that differ only in characters SPDX doesn't allow would collide: number them
    let mut taken = std::collections::BTreeMap::<String, usize>::new();
    let ids: Vec<String> = items.iter().map(|i| {
        let base = spdx_id(&i.id);
        let n = taken.entry(base.clone()).or_default();
        *n += 1;
        if *n > 1 { format!("{base}-{n}") } else { base }
    }).collect();
    for (i, id) in items.iter().zip(&ids) {
        let purpose = match i.ty {
            "application" => "APPLICATION",
            "library" => "LIBRARY",
            "file" => "FILE",
            _ => "OTHER",
        };
        let mut p = json!({
            "name": i.name,
            "SPDXID": id,
            "downloadLocation": i.source.clone().filter(|s| s.contains("://") || s.starts_with("git+")).unwrap_or_else(|| "NOASSERTION".into()),
            "filesAnalyzed": false,
            "primaryPackagePurpose": purpose,
            "licenseConcluded": "NOASSERTION",
            "licenseDeclared": i.license.clone().filter(|l| spdx_expression(l)).unwrap_or_else(|| "NOASSERTION".into()),
            "copyrightText": "NOASSERTION",
        });
        if let Some(v) = &i.version {
            p["versionInfo"] = json!(v);
        }
        if let Some(h) = i.digest.as_deref().and_then(hex) {
            p["checksums"] = json!([{"algorithm": "SHA256", "checksumValue": h}]);
        }
        if let Some(u) = &i.purl {
            p["externalRefs"] = json!([{"referenceCategory": "PACKAGE-MANAGER", "referenceType": "purl", "referenceLocator": u}]);
        }
        let mut notes: Vec<String> = i.properties.iter().map(|(k, v)| format!("{k}={v}")).collect();
        if i.ty == "machine-learning-model" {
            notes.insert(0, "machine-learning model".into());
        }
        if !notes.is_empty() {
            p["comment"] = json!(notes.join("; "));
        }
        packages.push(p);
        relationships.push(json!({"spdxElementId": "SPDXRef-stack", "relationshipType": "DEPENDS_ON", "relatedSpdxElement": id}));
    }
    json!({
        "spdxVersion": "SPDX-2.3",
        "dataLicense": "CC0-1.0",
        "SPDXID": "SPDXRef-DOCUMENT",
        "name": format!("{} (harness.lock)", stack_name(lock)),
        "documentNamespace": format!("https://docs.hlock.ai/spdx/{}", uuid_of(lock)),
        "creationInfo": {"created": when(lock), "creators": [format!("Tool: hlock-{}", env!("CARGO_PKG_VERSION"))]},
        "packages": packages,
        "relationships": relationships,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lockfile::{Harness, Package, Runtime, RuntimeModel, Skill, Tool, LLM};

    fn lock() -> Lockfile {
        Lockfile {
            generated_at: "2026-10-20T00:00:00Z".into(),
            harness: Harness { name: "claude-code".into(), version: Some("2.1.3".into()), source: Some("npm:@anthropic-ai/claude-code@2.1.3".into()), ..Default::default() },
            llm: vec![LLM { role: "default".into(), provider: "anthropic".into(), model: "claude-opus".into(), ..Default::default() }],
            runtime: vec![Runtime { name: "ollama".into(), location: "local".into(), version: Some("0.12.0".into()), model: vec![RuntimeModel { id: "qwen3:8b".into(), digest: Some(format!("sha256:{}", "a".repeat(64))), ..Default::default() }], ..Default::default() }],
            tool: vec![Tool { name: "github".into(), kind: "mcp".into(), source: Some("npm:@modelcontextprotocol/server-github@^1".into()), version: Some("1.4.0".into()), transport: Some("stdio".into()), ..Default::default() }],
            skill: vec![Skill { name: "pdf".into(), origin: "third-party".into(), source: Some("github:anthropics/skills/skills/pdf".into()), digest: Some(format!("sha256:{}", "b".repeat(64))), dep: crate::lockfile::Declared { commit: Some("c".repeat(40)), ..Default::default() }, ..Default::default() }],
            package: vec![Package { name: "ctx".into(), source: "apm:github/awesome-copilot/plugins/ctx".into(), license: Some("MIT".into()), depth: 1, ..Default::default() }],
            ..Default::default()
        }
    }

    #[test]
    fn cyclonedx_has_models_and_purls() {
        let b = cyclonedx(&lock());
        assert_eq!((b["bomFormat"].as_str(), b["specVersion"].as_str()), (Some("CycloneDX"), Some("1.6")));
        let comps = b["components"].as_array().unwrap();
        let by = |r: &str| comps.iter().find(|c| c["bom-ref"] == r).unwrap().clone();
        assert_eq!(by("harness:claude-code")["purl"], "pkg:npm/%40anthropic-ai/claude-code@2.1.3");
        assert_eq!(by("model:default:anthropic:claude-opus")["type"], "machine-learning-model");
        assert_eq!(by("model:ollama/qwen3:8b")["hashes"][0]["content"], "a".repeat(64));
        assert_eq!(by("mcp:github")["purl"], "pkg:npm/%40modelcontextprotocol/server-github@1.4.0");
        assert_eq!(by("skill:pdf")["purl"], format!("pkg:github/anthropics/skills@{}#skills/pdf", "c".repeat(40)));
        assert_eq!(by("package:ctx")["licenses"][0]["expression"], "MIT");
        assert_eq!(cyclonedx(&lock())["serialNumber"], b["serialNumber"], "the same lock, the same serial number");
    }

    #[test]
    fn spdx_has_valid_ids_and_relationships() {
        let d = spdx(&lock());
        assert_eq!(d["spdxVersion"], "SPDX-2.3");
        let pkgs = d["packages"].as_array().unwrap();
        assert!(pkgs.iter().all(|p| p["SPDXID"].as_str().unwrap().chars().all(|c| c.is_ascii_alphanumeric() || "-.".contains(c))));
        assert_eq!(d["relationships"].as_array().unwrap().len(), pkgs.len());
        let m = pkgs.iter().find(|p| p["name"] == "qwen3:8b").unwrap();
        assert!(m["comment"].as_str().unwrap().starts_with("machine-learning model"));
    }
}
