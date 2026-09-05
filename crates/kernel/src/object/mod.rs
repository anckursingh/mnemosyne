//! Object Manager — owns Knowledge Object read operations and head
//! resolution (MRFC-0005 §Knowledge Kernel).
//!
//! All KO reads route through here. The commit pipeline (write path)
//! stays in the Kernel orchestrator; this manager handles the read side.

use crate::knowledge::kom::*;
use crate::storage::repository::KnowledgeRepository;
use std::sync::Arc;

pub struct ObjectManager {
    repo: Arc<KnowledgeRepository>,
}

impl ObjectManager {
    pub fn new(repo: Arc<KnowledgeRepository>) -> Self {
        ObjectManager { repo }
    }

    /// Resolve the head pointer for a KOID.
    pub fn head(&self, koid: &KOID) -> KResult<Option<(u64, u64, LifecycleState)>> {
        self.repo.get_head(koid)
    }

    /// Load the current head version of a KO.
    pub fn get(&self, koid: &KOID) -> KResult<Option<KnowledgeObject>> {
        match self.repo.get_head(koid)? {
            Some((_version, ts, _state)) => self.repo.get_object_version(koid, ts),
            None => Ok(None),
        }
    }

    /// SE2-M25 — batch head-version loads: one head batch, then one version
    /// batch over the resolved timestamps. Fail-fast like the per-target
    /// path (a missing head or version is `KError::NotFound`, which is how
    /// `Kernel::get` surfaces an absent KO).
    pub fn get_many(&self, koids: &[KOID]) -> KResult<Vec<KnowledgeObject>> {
        let heads = self.repo.get_heads_many(koids)?;
        let mut wanted: Vec<(KOID, u64)> = Vec::with_capacity(koids.len());
        for (i, head) in heads.into_iter().enumerate() {
            let (_version, ts, _state) = head.ok_or(KError::NotFound(koids[i]))?;
            wanted.push((koids[i], ts));
        }
        let versions = self.repo.get_object_versions_many(&wanted)?;
        versions
            .into_iter()
            .enumerate()
            .map(|(i, v)| v.ok_or(KError::NotFound(wanted[i].0)))
            .collect()
    }

    /// Load a KO at a specific snapshot timestamp.
    pub fn get_at(&self, koid: &KOID, snap_ts: u64) -> KResult<Option<KnowledgeObject>> {
        self.repo.get_object_at(koid, snap_ts)
    }

    /// Load a KO at a specific commit timestamp (bypasses head pointer).
    /// Used by IndexMaintainer and other internal services.
    pub fn raw_at(&self, koid: &KOID, commit_ts: u64) -> KResult<Option<KnowledgeObject>> {
        self.repo.get_object_version(koid, commit_ts)
    }

    /// Enumerate all head pointers (KOID, version, ts, state).
    pub fn scan_heads(&self) -> KResult<Vec<(KOID, u64, u64, LifecycleState)>> {
        self.repo.scan_heads()
    }

    /// Enumerate all versions of a single KOID.
    pub fn scan_versions(&self, koid: &KOID) -> KResult<Vec<(u64, KnowledgeObject)>> {
        self.repo.scan_object_versions(koid)
    }
}
