//! PR-J HLD §16 Phase 3: transformer-backed boundary scorer behind the
//! `BoundaryScorer` seam.
//!
//! Feature-gated (`transform`) — never in the default build (HLD §56: no
//! mandatory heavyweight AI; DoD row 10: transformer optional). Talks to a
//! boundary-classifier HTTP endpoint, configured from the environment:
//!
//! - `AIKOQL_TRANSFORM_ENDPOINT` — base URL (required, e.g. `http://localhost:8080`)
//! - `AIKOQL_TRANSFORM_KEY` — bearer token (optional; local endpoints may need none)
//! - `AIKOQL_TRANSFORM_MODEL` — model id (default `transform-v1`)
//!
//! Usage: hand the scorer to `TransformerBoundaryDetector::new` (or
//! `HybridBoundaryDetector::with_scorer`) — the seams PR-I established.
//!
//! ```rust
//! use aikoql_ingestion::transform::{TransformConfig, TransformScorer};
//! use aikoql_ingestion::TransformerBoundaryDetector;
//!
//! let config = TransformConfig {
//!     endpoint: "http://localhost:8080".into(),
//!     api_key: None,
//!     model: "transform-v1".into(),
//! };
//! let scorer = TransformScorer::new(config);
//! let _detector = TransformerBoundaryDetector::new(&scorer);
//! ```

use crate::boundary::{BoundaryScore, BoundaryScorer};
use crate::chunking::fragment_text;
use crate::fragment::KnowledgeFragment;

/// Endpoint configuration from environment variables.
#[derive(Clone, Debug)]
pub struct TransformConfig {
    pub endpoint: String,
    pub api_key: Option<String>,
    pub model: String,
}

impl TransformConfig {
    /// `None` when `AIKOQL_TRANSFORM_ENDPOINT` is unset — the signal that no
    /// transformer is configured and the rule/mock pipeline should be used
    /// instead (DoD row 10: the transformer is optional).
    pub fn from_env() -> Option<Self> {
        let endpoint = std::env::var("AIKOQL_TRANSFORM_ENDPOINT").ok()?;
        Some(Self {
            endpoint,
            api_key: std::env::var("AIKOQL_TRANSFORM_KEY")
                .ok()
                .filter(|k| !k.is_empty()),
            model: std::env::var("AIKOQL_TRANSFORM_MODEL")
                .unwrap_or_else(|_| "transform-v1".into()),
        })
    }
}

/// HTTP boundary scorer: POSTs the two candidate halves to
/// `{endpoint}/boundary-score` as `{"prev": …, "next": …}` and reads
/// `{"probability": 0.94, "model": "…"}`. `None` on any transport/parse
/// failure — transformer output is untrusted and optional; the policy
/// threshold then leaves the boundary intact (HLD §17: the final decision
/// belongs to the boundary policy).
#[derive(Clone)]
pub struct TransformScorer {
    config: TransformConfig,
}

impl TransformScorer {
    pub fn new(config: TransformConfig) -> Self {
        Self { config }
    }
}

impl BoundaryScorer for TransformScorer {
    fn score_boundary(
        &self,
        prev: &KnowledgeFragment,
        next: &KnowledgeFragment,
    ) -> Option<BoundaryScore> {
        let body = serde_json::json!({
            "prev": fragment_text(prev),
            "next": fragment_text(next),
        });
        let mut req = ureq::post(&format!("{}/boundary-score", self.config.endpoint))
            .header("Content-Type", "application/json");
        if let Some(key) = &self.config.api_key {
            req = req.header("Authorization", &format!("Bearer {}", key));
        }
        let resp = req.send_json(body).ok()?;
        let v: serde_json::Value = resp.into_body().read_json().ok()?;
        let probability = v["probability"].as_f64()? as f32;
        let model = v["model"]
            .as_str()
            .unwrap_or(&self.config.model)
            .to_string();
        Some(BoundaryScore { probability, model })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_requires_endpoint_env() {
        std::env::remove_var("AIKOQL_TRANSFORM_ENDPOINT");
        assert!(TransformConfig::from_env().is_none());

        std::env::set_var("AIKOQL_TRANSFORM_ENDPOINT", "http://localhost:8080");
        std::env::remove_var("AIKOQL_TRANSFORM_KEY");
        std::env::remove_var("AIKOQL_TRANSFORM_MODEL");
        let config = TransformConfig::from_env().expect("endpoint set");
        assert_eq!(config.endpoint, "http://localhost:8080");
        assert!(config.api_key.is_none());
        assert_eq!(config.model, "transform-v1"); // default

        std::env::set_var("AIKOQL_TRANSFORM_KEY", "k");
        std::env::set_var("AIKOQL_TRANSFORM_MODEL", "custom-model");
        let config = TransformConfig::from_env().expect("endpoint set");
        assert_eq!(config.api_key.as_deref(), Some("k"));
        assert_eq!(config.model, "custom-model");

        std::env::remove_var("AIKOQL_TRANSFORM_ENDPOINT");
        std::env::remove_var("AIKOQL_TRANSFORM_KEY");
        std::env::remove_var("AIKOQL_TRANSFORM_MODEL");
    }
}
