//! LLM providers — a thin abstraction with a deterministic reference provider.

/// A provider manifests itself to the resolver through `complete`, which returns a
/// `ProviderResponse`. The reference provider is deterministic and offline so the
/// whole local loop runs with no accounts or network.
#[derive(Debug, Clone)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug, Clone)]
pub struct ProviderResponse {
    pub content: String,
    pub usage: Usage,
    pub cost_usd: f64,
    pub latency_ms: f64,
}

/// Deterministic provider: "succeeds" with per-model probability so the resolver can
/// discriminate candidate stacks and produce a real-looking leaderboard + lock.
#[derive(Debug, Clone)]
pub struct ReferenceProvider {
    pub(crate) seed: u64,
}

impl Default for ReferenceProvider {
    fn default() -> Self {
        ReferenceProvider { seed: 0 }
    }
}

impl ReferenceProvider {
    /// Resolve a `provider:model` manifest spec to a pinned model id.
    pub fn resolve_alias(&self, alias: &str) -> (String, String) {
        match alias.split_once(':') {
            Some((p, m)) if !m.is_empty() => (p.to_string(), m.to_string()),
            _ => ("reference".to_string(), alias.to_string()),
        }
    }

    fn quality(model: &str) -> f64 {
        match model {
            "qwen-1.5b" => 0.55,
            "qwen-7b" => 0.75,
            "qwen-32b" => 0.88,
            _ => {
                // deterministic pseudo-quality from the model id
                let h = sha256_int(model);
                0.4 + (h % 5000) as f64 / 10000.0
            }
        }
    }

    pub fn complete(&self, model: &str, messages: u64) -> ProviderResponse {
        // simple deterministic rng based on seed + prompt length
        let mut state = self.seed * 2654435761 ^ (messages as u64).wrapping_mul(40503);
        for _ in 0..4 {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        }
        let r = (state >> 33) as f64 / 4294967296.0_f64;
        let q = Self::quality(model);
        let succeeded = r < q;
        let content = format!(
            "{}{}|{}",
            if succeeded { "ANSWER_OK" } else { "ANSWER_UNCERTAIN" },
            "",
            model
        );
        let cost: f64 = 0.0001 * if model.contains("32") { 4.0 } else { 1.0 };
        let latency = 100.0 + (r * 800.0);
        ProviderResponse {
            content,
            usage: Usage {
                input_tokens: 200,
                output_tokens: 50,
            },
            cost_usd: (cost * 1_000_000.0_f64).round() / 1_000_000.0_f64,
            latency_ms: (latency * 10.0).round() / 10.0,
        }
    }
}

fn sha256_int(s: &str) -> u64 {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    let d = h.finalize();
    let mut out = 0u64;
    for b in &d[..8] {
        out = (out << 8) | *b as u64;
    }
    out
}
