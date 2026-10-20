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
        self.respond(model, self.seed.wrapping_mul(2654435761) ^ messages.wrapping_mul(40503))
    }

    /// One call in a run of task `task`: attempt `attempt` at step `step`. Each call
    /// draws afresh, so a retry or a later step can succeed where an earlier one failed,
    /// and the same `(seed, task, step, attempt)` always answers the same way.
    pub fn complete_for(&self, model: &str, task: &str, step: u64, attempt: u64) -> ProviderResponse {
        let state = mix(self.seed.wrapping_mul(2654435761) ^ mix(sha256_int(task) ^ mix(step.wrapping_mul(40503) ^ mix(attempt))));
        self.respond(model, state)
    }

    fn respond(&self, model: &str, mut state: u64) -> ProviderResponse {
        for _ in 0..4 {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        }
        // uniform in [0, 1): the top 32 bits (a 31-bit shift only ever reached 0.5)
        let r = (state >> 32) as f64 / 4294967296.0_f64;
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

/// The splitmix64 finalizer: spreads every input bit over the output.
fn mix(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calls_vary_by_step_attempt_and_task_and_repeat_exactly() {
        let p = ReferenceProvider { seed: u64::MAX }; // a large seed doesn't overflow
        let ok = |task: &str, step: u64, attempt: u64| p.complete_for("qwen-1.5b", task, step, attempt).content.starts_with("ANSWER_OK");
        let draws: Vec<bool> = (0..32).map(|a| ok("t", 0, a)).collect();
        assert!(draws.contains(&true) && draws.contains(&false), "retries draw afresh: {draws:?}");
        assert_ne!((0..32).map(|s| ok("t", s, 0)).collect::<Vec<_>>(), (0..32).map(|s| ok("u", s, 0)).collect::<Vec<_>>());
        assert_eq!(p.complete_for("qwen-7b", "t", 3, 1).latency_ms, p.complete_for("qwen-7b", "t", 3, 1).latency_ms);
        let _ = p.complete("qwen-7b", u64::MAX);
    }
}
