//! The Knowledge Kernel: commit pipeline + KS-ABI Class A syscalls (MRFC-0011).
//!
//! Design invariants (conformance-tested):
//! - Single-writer pipeline: one mutex serializes validation -> OCC -> HLC
//!   assignment -> atomic write batch (KO version + KE + journal head) -> ack.
//!   This is where "zero committed loss" is won (review §4.3).
//! - MVCC: versions keyed by (koid, commit_ts); readers pin a snapshot ts.
//! - Determinism Law (MRFC-0011 §7): no wall-clock reads except via the
//!   injected `Clock`; no external calls anywhere in this file.
//! - ACL enforcement lives HERE (kernel boundary), not in adapters (MRFC-0001 §12).
//! - R4 remediation: all `.lock().unwrap()` on Mutex/RwLock are **justified** —
//!   Mutex poisoning means another thread panicked while holding the lock, the
//!   process is already unrecoverable, and crashing is the correct response.
//!   Similarly, `.as_ref().unwrap()` on head pointers inside `remember()` is
//!   **justified** — the preceding branch guarantees Some; a None here would be
//!   a logic bug that should crash. The two `.map(...).unwrap_or_default()`
//!   sites (scan/head-summary defaults) and the coordinator `.expect()` are
//!   also **justified** — inline `// justified:` comments mark each.

use crate::embedding::EmbeddingProvider;
use crate::event::EventManager;
use crate::index::coordinator::IndexCoordinator;
use crate::knowledge::authority::Authority;
use crate::knowledge::codec::{self, Enc};
use crate::knowledge::kom::*;
use crate::knowledge::ontology::{Cardinality, OntologyRegistry};
use crate::knowledge::scope::Scope;
use crate::lifecycle::constraint::{ConstraintEvaluator, InferenceEngine};
use crate::lifecycle::schema::SchemaRegistry;
use crate::object::ObjectManager;
use crate::relationship::RelationshipManager;
use crate::security::auth::{AuthManager, POLICY_TYPE, ROLE_TYPE};
use crate::security::crypto::Crypto;
use crate::security::envelope::{Envelope, CRYPTO_META_KEY, CRYPTO_META_V1, DEKS_STORAGE_KEY};
use crate::security::field_crypto::{ComplianceSummary, EncryptionPolicy, FieldCrypto};
use crate::security::tenant::TenantManager;
pub use crate::storage::repository::DerivedIndexRebuild;
use crate::storage::repository::KnowledgeRepository;
use crate::storage::store::{ConstraintCapabilities, StorageEngine, WriteBatch};
use std::collections::{HashMap, HashSet};
use std::sync::{mpsc, Arc, Mutex, RwLock};

// v0.3 K4: knowledge transactions (observe/assert/verify/contradict/supersede/
// merge/invalidate + conflict resolution). A child module so the ops share
// kernel.rs's private fields (pipe/auth/clock) without widening their scope.
mod ops;
pub use ops::*;

// ---------------------------------------------------------------------------
// Clock & Hybrid Logical Clock (commit timestamps)
// ---------------------------------------------------------------------------

pub trait Clock: Send + Sync {
    fn millis(&self) -> u64;
}

pub struct SystemClock;
impl Clock for SystemClock {
    fn millis(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
}

/// Deterministic clock for conformance replay.
pub struct ManualClock {
    now: Mutex<u64>,
}
impl ManualClock {
    pub fn new(t: u64) -> Self {
        ManualClock { now: Mutex::new(t) }
    }
    pub fn set(&self, t: u64) {
        *self.now.lock().unwrap() = t;
    }
    pub fn tick(&self, d: u64) {
        *self.now.lock().unwrap() += d;
    }
}
impl Clock for ManualClock {
    fn millis(&self) -> u64 {
        *self.now.lock().unwrap()
    }
}

/// HLC packed as (millis << 16) | counter. Monotone even under clock regression.
struct Hlc {
    last: Mutex<u64>,
}
impl Hlc {
    #[cfg(test)]
    fn new() -> Self {
        Hlc {
            last: Mutex::new(0),
        }
    }
    /// Re-seed from the persisted journal head so commit timestamps stay
    /// monotone across process restarts (durability requirement).
    fn starting_at(ts: u64) -> Self {
        Hlc {
            last: Mutex::new(ts),
        }
    }
    fn now(&self, clock: &dyn Clock) -> u64 {
        let mut last = self.last.lock().unwrap();
        let ms = clock.millis();
        let cur_ms = *last >> 16;
        *last = if ms > cur_ms { ms << 16 } else { *last + 1 };
        *last
    }
    fn current(&self) -> u64 {
        *self.last.lock().unwrap()
    }
}

// ---------------------------------------------------------------------------
// Audit-chain preimage: covers every field an attacker might flip.
// ---------------------------------------------------------------------------
fn audit_hash_of(
    prev: [u8; 32],
    seq: u64,
    koid: &KOID,
    version: u64,
    kind: EventKind,
    commit_ts: u64,
    payload_hash: &[u8; 32],
    signature: Option<&[u8; 32]>,
    actor: &str,
    note: Option<&str>,
) -> [u8; 32] {
    let mut e = Enc::new();
    e.hash256(&prev);
    e.u64(seq);
    e.raw(koid.as_bytes());
    e.u64(version);
    e.u8(kind.tag());
    e.u64(commit_ts);
    e.hash256(payload_hash);
    e.opt_hash256(signature);
    e.str(actor);
    e.opt_str(note);
    sha256(&e.buf)
}

/// Check whether a uniqueness conflict exists in storage, respecting `UniquenessScope`.
/// MRFC-0060 AC-05: scope-aware lookup used by `check_uniqueness` and `evaluate_deferred`.
fn uniqueness_conflict(
    objects: &ObjectManager,
    scope: crate::kom::UniquenessScope,
    tenant: Option<&str>,
    type_name: &str,
    pairs: &[(String, crate::kom::Value)],
    exclude_koid: &crate::kom::KOID,
) -> bool {
    let Ok(heads) = objects.scan_heads() else {
        return false;
    };
    for (hkoid, _version, _ts, state) in &heads {
        if hkoid == exclude_koid {
            continue;
        }
        if *state == LifecycleState::Deleted {
            continue;
        }
        let Ok(Some(existing)) = objects.get(hkoid) else {
            continue;
        };
        // Scope-aware matching (MRFC-0060 AC-05).
        let in_scope = match scope {
            crate::kom::UniquenessScope::Type => existing.metadata.type_name == type_name,
            crate::kom::UniquenessScope::Tenant => match tenant {
                Some(t) => existing.metadata.tenant.as_deref() == Some(t),
                None => false, // un-tenanted: no Tenant-scope conflicts
            },
            crate::kom::UniquenessScope::Global => true,
        };
        if !in_scope {
            continue;
        }
        let all_match = pairs
            .iter()
            .all(|(pn, pv)| existing.properties.get(pn.as_str()) == Some(pv));
        if all_match {
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Public request/response types (KS-ABI Class A)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Subject {
    pub name: String,
    pub roles: Vec<String>,
    /// R9: tenant scope confinement. When set, `authorize` denies access to
    /// objects in any other tenant (untenanted objects remain shared). `None`
    /// means unscoped — the pre-R9 single-tenant behavior, unchanged.
    pub tenant: Option<String>,
}

impl Subject {
    pub fn new(name: &str) -> Self {
        Subject {
            name: name.into(),
            roles: vec![],
            tenant: None,
        }
    }
    pub fn with_roles(name: &str, roles: &[&str]) -> Self {
        Subject {
            name: name.into(),
            roles: roles.iter().map(|r| r.to_string()).collect(),
            tenant: None,
        }
    }
    /// Confine this subject to one tenant (R9).
    pub fn in_tenant(mut self, tenant: impl Into<String>) -> Self {
        self.tenant = Some(tenant.into());
        self
    }
    pub(crate) fn is_admin(&self) -> bool {
        self.roles.iter().any(|r| r == "admin")
    }
}

/// Runtime context carried by every kernel operation.
///
/// Groups identity, tenancy, and snapshot so syscalls do not accumulate a long
/// parameter list. The kernel uses `subject` (including its R9 tenant scope),
/// `snapshot`, and — for field-level crypto key derivation — `tenant`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KnowledgeContext {
    pub subject: Subject,
    pub tenant: Option<String>,
    pub agent: Option<String>,
    pub reasoning_mode: Option<String>,
    pub snapshot: Option<u64>,
}

impl KnowledgeContext {
    pub fn new(subject: Subject) -> Self {
        Self {
            subject,
            tenant: None,
            agent: None,
            reasoning_mode: None,
            snapshot: None,
        }
    }

    pub fn with_tenant(mut self, tenant: impl Into<String>) -> Self {
        self.tenant = Some(tenant.into());
        self
    }

    pub fn with_agent(mut self, agent: impl Into<String>) -> Self {
        self.agent = Some(agent.into());
        self
    }

    pub fn with_reasoning_mode(mut self, mode: impl Into<String>) -> Self {
        self.reasoning_mode = Some(mode.into());
        self
    }

    pub fn with_snapshot(mut self, snapshot: u64) -> Self {
        self.snapshot = Some(snapshot);
        self
    }
}

impl From<Subject> for KnowledgeContext {
    fn from(subject: Subject) -> Self {
        Self::new(subject)
    }
}

impl From<&Subject> for KnowledgeContext {
    fn from(subject: &Subject) -> Self {
        Self::new(subject.clone())
    }
}

#[derive(Clone, Debug)]
pub struct RememberRequest {
    pub context: KnowledgeContext,
    /// None => create new KO.
    pub koid: Option<KOID>,
    /// OCC guard. Create: must be None/0. Update: defaults to current head.
    pub expected_version: Option<u64>,
    /// Retried calls with the same key commit exactly once (MRFC-0011 req 10).
    pub idempotency_key: Option<String>,
    pub metadata: Metadata,
    pub properties: PropertyMap,
    pub semantic: Option<SemanticBlock>,
    pub relationships: Vec<RelationshipRef>,
    /// Create: defaults to owner=subject. Update: None keeps existing.
    pub security: Option<SecurityDescriptor>,
    pub extensions: ExtensionMap,
    pub origin: Origin,
    pub note: Option<String>,
    pub referential_policy: ReferentialPolicy,
}

impl RememberRequest {
    pub fn create(context: impl Into<KnowledgeContext>, metadata: Metadata) -> Self {
        RememberRequest {
            context: context.into(),
            koid: None,
            // insert-only semantics: conflicts deterministically if the KOID exists
            expected_version: Some(0),
            idempotency_key: None,
            metadata,
            properties: PropertyMap::new(),
            semantic: None,
            relationships: vec![],
            security: None,
            extensions: ExtensionMap::new(),
            origin: Origin::Human,
            note: None,
            referential_policy: ReferentialPolicy::default(),
        }
    }
    pub fn update(context: impl Into<KnowledgeContext>, koid: KOID, metadata: Metadata) -> Self {
        RememberRequest {
            context: context.into(),
            koid: Some(koid),
            expected_version: None,
            idempotency_key: None,
            metadata,
            properties: PropertyMap::new(),
            semantic: None,
            relationships: vec![],
            security: None,
            extensions: ExtensionMap::new(),
            origin: Origin::Human,
            note: None,
            referential_policy: ReferentialPolicy::default(),
        }
    }
}

/// One operation inside a multi-object transaction.
#[derive(Clone, Debug)]
pub struct TransactionOp {
    pub context: KnowledgeContext,
    pub request: RememberRequest,
}

impl TransactionOp {
    pub fn new(context: impl Into<KnowledgeContext>, request: RememberRequest) -> Self {
        TransactionOp {
            context: context.into(),
            request,
        }
    }
}

/// Compliance report for encryption audit (MRFC-0020 Phase 4).
#[derive(Clone, Debug)]
pub struct ComplianceReport {
    pub encryption_enabled: bool,
    pub policies_registered: usize,
    pub policy_types: Vec<String>,
    pub field_crypto_summary: Option<ComplianceSummary>,
}

/// MRFC-0020 Phase 4: retention evidence for the compliance evidence pack.
/// Counts of heads carrying a kernel-stamped `valid_to` horizon
/// (`remember_retained`), split by expiry against the kernel clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetentionSummary {
    /// Heads carrying EXT_VALID_TO — objects under declarative retention.
    pub retained_objects: usize,
    /// Horizons still in the future (live retention windows).
    pub live_windows: usize,
    /// Horizons at or past the kernel clock — purge-eligible under the
    /// half-open validity interval.
    pub expired: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Remembered {
    pub koid: KOID,
    pub version: u64,
    pub commit_ts: u64,
}

/// v0.3 K3: a first-class derivation request — the anti-CRUD-cosplay form of
/// "create a KO that came from other KOs" (review H6: a bare write of a
/// DERIVED_FROM edge is not a derivation).
#[derive(Clone, Debug)]
pub struct DeriveRequest {
    pub context: KnowledgeContext,
    /// Type of the derived KO.
    pub type_name: String,
    pub properties: PropertyMap,
    /// Premise KOs — all must exist and be readable; each is wired as an
    /// inbound DERIVED_FROM edge on the derived KO, so `outbound_edges(src,
    /// "derived_from")` finds every dependent (K4 invalidation input).
    pub sources: Vec<KOID>,
    /// The derivation operation (rule_fired, inference, merge, extraction…).
    pub operation: String,
    /// Who (or which agent) performed the derivation.
    pub actor: String,
    /// The model used, if the derivation was model-assisted.
    pub model: Option<String>,
    /// Human-readable justification — the WHY.
    pub reason: Option<String>,
    /// Structured evidence trail (canonical Evidence extension).
    pub evidence: Vec<crate::knowledge::evidence::Evidence>,
    /// Confidence context override; None derives a baseline from the sources.
    pub confidence: Option<ConfidenceContext>,
}

impl DeriveRequest {
    pub fn new(context: impl Into<KnowledgeContext>, type_name: impl Into<String>) -> Self {
        let ctx = context.into();
        DeriveRequest {
            context: ctx.clone(),
            type_name: type_name.into(),
            properties: PropertyMap::new(),
            sources: Vec::new(),
            operation: "derivation".into(),
            actor: ctx.subject.name,
            model: None,
            reason: None,
            evidence: Vec::new(),
            confidence: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Evolved {
    pub koid: KOID,
    pub version: u64,
    pub commit_ts: u64,
    pub state: LifecycleState,
}

/// v0.3 K1: result of an epistemic status transition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EpistemicChanged {
    pub koid: KOID,
    pub version: u64,
    pub commit_ts: u64,
    pub from: EpistemicStatus,
    pub to: EpistemicStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Forgotten {
    pub koid: KOID,
    pub version: u64,
    pub commit_ts: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ForgetMode {
    Tombstone,
    Erase,
}

#[derive(Clone, Debug, Default)]
pub struct PropertyFilter {
    pub type_name: Option<String>,
    pub required: Vec<(String, Value)>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Fusion {
    VectorOnly,
    TextOnly,
    Weighted {
        wv: f32,
        wt: f32,
    },
    Rrf {
        k0: u32,
    },
    /// Bypass indexes entirely — exact scan-and-filter (MRFC-0009 §4).
    Exact,
}

#[derive(Clone, Debug)]
pub struct SimilarityQuery {
    pub context: KnowledgeContext,
    pub filter: Option<PropertyFilter>,
    pub text: Option<String>,
    pub vector: Option<Vec<f32>>,
    /// When set, only vectors from this embedding model are considered.
    /// When `None`, all models are searched (backward-compatible).
    pub embedding_model: Option<String>,
    pub k: usize,
    pub fusion: Fusion,
}

impl SimilarityQuery {
    pub fn new(context: impl Into<KnowledgeContext>, k: usize, fusion: Fusion) -> Self {
        Self {
            context: context.into(),
            filter: None,
            text: None,
            vector: None,
            embedding_model: None,
            k,
            fusion,
        }
    }

    pub fn with_text(mut self, text: impl Into<String>) -> Self {
        self.text = Some(text.into());
        self
    }

    pub fn with_vector(mut self, vector: Vec<f32>) -> Self {
        self.vector = Some(vector);
        self
    }

    pub fn with_filter(mut self, filter: PropertyFilter) -> Self {
        self.filter = Some(filter);
        self
    }
}

#[derive(Clone, Debug)]
pub struct ScoredKO {
    pub ko: KnowledgeObject,
    pub score: f32,
    /// Staleness of any consulted index. Inc-1 computes exact inline scores
    /// over committed state, so lag is 0; async indexes (Inc-2) report real lag.
    pub index_lag_ms: u64,
}

#[derive(Clone, Debug)]
pub struct VersionRecord {
    pub version: u64,
    pub commit_ts: u64,
    pub origin: Origin,
    pub state: LifecycleState,
}

#[derive(Clone, Debug)]
pub struct Lineage {
    pub koid: KOID,
    pub versions: Vec<VersionRecord>,
    pub events: Vec<KnowledgeEvent>,
}

#[derive(Clone, Debug)]
pub struct Explanation {
    pub koid: KOID,
    pub version: u64,
    pub origin: Origin,
    pub source: Option<String>,
    pub confidence: Option<f32>,
    pub verified: bool,
    pub evidence: Vec<(String, KOID)>,
    pub event_refs: Vec<EventRef>,
}

#[derive(Clone, Debug)]
pub struct Proof {
    pub claim: KOID,
    pub events: u64,
    pub chain_valid: bool,
    pub head_audit_hash: [u8; 32],
    /// True when all KEs carrying a version signature verified against the
    /// configured signing key (or when no signatures are present).
    pub signatures_verified: bool,
}

pub use crate::knowledge::notify::{EventFilter, SubscriptionRecord};

// ---------------------------------------------------------------------------
// Kernel
// ---------------------------------------------------------------------------

pub(crate) struct Pipeline {
    seq: u64,
    audit: [u8; 32],
}

pub struct Kernel {
    repo: Arc<KnowledgeRepository>,
    /// Raw store handle for keys outside the repository (encryption DEKs).
    store: Arc<dyn StorageEngine>,
    clock: Arc<dyn Clock>,
    hlc: Arc<Hlc>,
    idgen: Arc<Mutex<IdGen>>,
    pipe: Arc<Mutex<Pipeline>>,
    events: Arc<Mutex<EventManager>>,
    auth: Arc<RwLock<AuthManager>>,
    indexes: Arc<RwLock<Option<Arc<IndexCoordinator>>>>,
    schemas: Arc<RwLock<SchemaRegistry>>,
    ontologies: Arc<RwLock<OntologyRegistry>>,
    constraint_eval: ConstraintEvaluator,
    /// Backend-native constraint capabilities snapshot at open time (C7).
    constraint_caps: ConstraintCapabilities,
    relationships: Arc<RelationshipManager>,
    objects: Arc<ObjectManager>,
    /// Optional 32-byte HMAC-SHA256 key for at-rest version signatures.
    signing_key: Option<[u8; 32]>,
    /// Per-tenant quota tracking and enforcement.
    tenants: Arc<TenantManager>,
    /// Optional field-level encryption (MRFC-0020 Phase 3).
    field_crypto: Option<Arc<FieldCrypto>>,
    /// Per-type encryption policies: type_name → which fields to encrypt.
    encryption_policies: Arc<RwLock<HashMap<String, EncryptionPolicy>>>,
    /// Optional embedding provider for query-time ANN search (USING EMBEDDING).
    embedding_provider: Option<Arc<dyn EmbeddingProvider>>,
}

impl Kernel {
    /// Open (or create) a kernel over `store`. Recovers the journal head so a
    /// restarted kernel continues the hash chain and sequence numbers.
    /// `id_seed` namespaces this kernel's KOID id-space (encoded into every
    /// KOID); it is not cryptographic material.
    pub fn open(
        store: Arc<dyn StorageEngine>,
        clock: Arc<dyn Clock>,
        id_seed: u64,
    ) -> KResult<Self> {
        let repo = Arc::new(KnowledgeRepository::new(store.clone()));
        // R9: one-time backfill of the type index for databases created before
        // it existed. Marker makes it a no-op on every subsequent open.
        if !repo.type_index_marker()? {
            let mut batch = WriteBatch::new();
            let mut indexed = 0usize;
            for (koid, _version, ts, state) in repo.scan_heads()? {
                if state == LifecycleState::Deleted {
                    continue;
                }
                if let Some(ko) = repo.get_object_version(&koid, ts)? {
                    repo.write_type_index(&mut batch, &ko.metadata.type_name, &koid);
                    indexed += 1;
                }
            }
            repo.put_type_index_marker(&mut batch);
            repo.write_batch(&batch)?;
            if indexed > 0 {
                eprintln!("type index: backfilled {indexed} objects");
            }
        }
        let (seq, audit, last_ts) = match repo.journal_head()? {
            Some((s, a, t)) => (s, a, t),
            None => (0, [0u8; 32], 0),
        };
        let events = EventManager::load(&repo)?;
        let auth = AuthManager::load(&repo)?;
        let relationships = Arc::new(RelationshipManager::new(repo.clone()));
        let objects = Arc::new(ObjectManager::new(repo.clone()));
        let constraint_caps = repo.constraint_capabilities();
        // REC-002: reload persisted schemas. Fail closed — a corrupt schema row
        // must not silently drop constraints.
        let schemas = Arc::new(RwLock::new(SchemaRegistry::new()));
        for (type_name, bytes) in repo.schema_rows()? {
            let schema = crate::knowledge::codec::decode_schema(&bytes).map_err(|e| {
                KError::Store(format!("persisted schema '{}' corrupt: {}", type_name, e))
            })?;
            schemas.write().unwrap().register(schema);
        }
        Ok(Kernel {
            repo,
            store,
            clock,
            hlc: Arc::new(Hlc::starting_at(last_ts)),
            idgen: Arc::new(Mutex::new(IdGen::new(id_seed))),
            pipe: Arc::new(Mutex::new(Pipeline { seq, audit })),
            events: Arc::new(Mutex::new(events)),
            auth: Arc::new(RwLock::new(auth)),
            indexes: Arc::new(RwLock::new(Some(IndexCoordinator::new()))),
            schemas,
            ontologies: Arc::new(RwLock::new(OntologyRegistry::empty())),
            constraint_eval: ConstraintEvaluator::new(),
            constraint_caps,
            relationships,
            objects,
            signing_key: None,
            tenants: Arc::new(TenantManager::new()),
            field_crypto: None,
            encryption_policies: Arc::new(RwLock::new(HashMap::new())),
            embedding_provider: None,
        })
    }

    /// Enable at-rest HMAC-SHA256 version signatures. Idempotent and safe to
    /// call on a `clone_handle` before handing to another subsystem.
    pub fn with_signing_key(mut self, key: [u8; 32]) -> Self {
        self.signing_key = Some(key);
        self
    }

    /// Enable an in-memory LRU cache for heads and object versions.
    /// Only effective when called on the originally opened kernel (before any
    /// `clone_handle` shares the repository).
    pub fn with_cache(mut self, capacity: usize) -> Self {
        if let Some(repo) = Arc::get_mut(&mut self.repo) {
            repo.with_cache(capacity);
        }
        self
    }

    /// Enable field-level encryption (MRFC-0020 Phase 3).
    /// Only effective when called on the originally opened kernel.
    /// Loads persisted wrapped DEKs from the store. Fails closed on a corrupt
    /// DEK record: continuing would mint a fresh DEK for the tenant and orphan
    /// every field-encrypted value.
    pub fn with_field_encryption(
        mut self,
        crypto: Arc<Crypto>,
        envelope: Arc<Envelope>,
    ) -> KResult<Self> {
        // Crypto-version metadata: written on first open, verified on every
        // later open. An unknown record fails closed — we never silently
        // guess key material against a different crypto scheme.
        if let Some(meta) = self.store.get(CRYPTO_META_KEY)? {
            if meta != CRYPTO_META_V1 {
                return Err(KError::Store(format!(
                    "unsupported crypto metadata version: {:?}",
                    String::from_utf8_lossy(&meta)
                )));
            }
        } else {
            let mut batch = WriteBatch::new();
            batch.put(CRYPTO_META_KEY.to_vec(), CRYPTO_META_V1.to_vec());
            self.store
                .write_batch(&batch)
                .map_err(|e| KError::Store(format!("crypto meta persist: {}", e)))?;
        }
        if let Some(raw) = self.store.get(DEKS_STORAGE_KEY)? {
            let deks = Envelope::decode_wrapped_deks(&raw)
                .map_err(|e| KError::Store(format!("DEK load: {}", e)))?;
            for d in &deks {
                envelope
                    .load_dek(d)
                    .map_err(|e| KError::Store(format!("DEK load: {}", e)))?;
            }
        }
        self.field_crypto = Some(Arc::new(FieldCrypto::new(crypto, envelope)));
        Ok(self)
    }

    /// Register an encryption policy for a schema type. Fields listed in the
    /// policy are encrypted on `remember` and decrypted on `get`.
    pub fn set_encryption_policy(&self, type_name: &str, policy: EncryptionPolicy) {
        self.encryption_policies
            .write()
            .unwrap()
            .insert(type_name.to_string(), policy);
    }

    /// Remove an encryption policy.
    pub fn remove_encryption_policy(&self, type_name: &str) {
        self.encryption_policies.write().unwrap().remove(type_name);
    }

    /// Generate a compliance report for encryption audit (MRFC-0020 Phase 4).
    /// Returns encryption status, policy inventory, and key audit event counts.
    pub fn compliance_report(&self) -> KResult<ComplianceReport> {
        let pol_count = self.encryption_policies.read().unwrap().len();
        let pol_types: Vec<String> = self
            .encryption_policies
            .read()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        let summary = self.field_crypto.as_ref().map(|fc| fc.compliance_summary());
        Ok(ComplianceReport {
            encryption_enabled: self.field_crypto.is_some(),
            policies_registered: pol_count,
            policy_types: pol_types,
            field_crypto_summary: summary.transpose().unwrap_or(None),
        })
    }

    /// Retention evidence (MRFC-0020 Phase 4): count the kernel-stamped
    /// `valid_to` horizons across all heads, split by expiry. Reads heads
    /// raw (no ACL) — this is auditor evidence, gated at the tool layer.
    pub fn retention_summary(&self) -> KResult<RetentionSummary> {
        let now = self.clock_now();
        let mut summary = RetentionSummary {
            retained_objects: 0,
            live_windows: 0,
            expired: 0,
        };
        for (koid, _version, _ts, _state) in self.scan_heads()? {
            let Some(ko) = self.head_object(&koid)? else {
                continue;
            };
            let Some(valid_to) = ko.valid_to() else {
                continue;
            };
            summary.retained_objects += 1;
            if valid_to > now {
                summary.live_windows += 1;
            } else {
                summary.expired += 1;
            }
        }
        Ok(summary)
    }

    pub fn new_koid(&self) -> KOID {
        self.idgen.lock().unwrap().next(self.clock.millis())
    }

    /// Current read timestamp (snapshot isolation anchor, MRFC-0001 §8).
    pub fn snapshot(&self) -> u64 {
        self.hlc.current()
    }

    /// Shared-state handle for auxiliary subsystems (e.g. index maintainer
    /// threads). All pipeline state is shared — commits stay single-writer.
    pub fn clone_handle(&self) -> Kernel {
        Kernel {
            repo: self.repo.clone(),
            store: self.store.clone(),
            clock: self.clock.clone(),
            hlc: self.hlc.clone(),
            idgen: self.idgen.clone(),
            pipe: self.pipe.clone(),
            events: self.events.clone(),
            auth: self.auth.clone(),
            indexes: self.indexes.clone(),
            schemas: self.schemas.clone(),
            ontologies: self.ontologies.clone(),
            constraint_eval: self.constraint_eval.clone(),
            constraint_caps: self.constraint_caps,
            relationships: self.relationships.clone(),
            objects: self.objects.clone(),
            signing_key: self.signing_key,
            tenants: self.tenants.clone(),
            field_crypto: self.field_crypto.clone(),
            encryption_policies: self.encryption_policies.clone(),
            embedding_provider: self.embedding_provider.clone(),
        }
    }

    /// Attach an index maintainer; `find_similar` routes through it afterwards.
    pub fn attach_indexes(&self, m: Arc<dyn crate::index::IndexMaintainerApi>) {
        *self.indexes.write().unwrap() = Some(IndexCoordinator::with_maintainer(m));
    }

    /// Builder: attach an embedding provider for query-time ANN search
    /// (`MATCH ... USING EMBEDDING`).  Call after `Kernel::open()`.
    pub fn with_embedding_provider(mut self, p: Arc<dyn EmbeddingProvider>) -> Self {
        self.embedding_provider = Some(p);
        self
    }

    /// Embed query text using the configured provider.  Returns
    /// `UnsupportedOperation` when no provider is wired.
    pub fn embed_text(&self, text: &str, model: Option<&str>) -> KResult<Vec<f32>> {
        match &self.embedding_provider {
            Some(p) => p.embed(text, model),
            None => Err(KError::UnsupportedOperation(
                "no embedding provider configured — run `aikoql model install` for offline \
                 embeddings or set --embedding-provider openai"
                    .into(),
            )),
        }
    }

    /// Register a schema for automatic validation on `remember`.
    /// Persisted as a reserved row (REC-002) so backup/restore preserves
    /// constraints. A store failure leaves the registry unchanged.
    pub fn register_schema(&self, schema: Schema) -> KResult<()> {
        let bytes = crate::knowledge::codec::encode_schema(&schema);
        let mut batch = WriteBatch::new();
        self.repo
            .put_schema_row(&mut batch, &schema.type_name, &bytes);
        self.repo.write_batch(&batch)?;
        self.schemas.write().unwrap().register(schema);
        Ok(())
    }

    /// Register an ontology for relationship validation (MRFC-0060 Phase C3).
    pub fn register_ontology(&self, registry: OntologyRegistry) {
        *self.ontologies.write().unwrap() = registry;
    }

    /// Validate that existing data satisfies a proposed new schema (MRFC-0060 AC-22).
    ///
    /// Scans all committed objects of `new_schema.type_name` and runs every
    /// constraint (domain, check, unique) against each one.  Returns violations
    /// keyed by KOID so the caller can decide whether to proceed with the migration.
    pub fn validate_schema_migration(
        &self,
        _subject: &Subject,
        new_schema: &Schema,
    ) -> KResult<Vec<crate::kom::ConstraintViolation>> {
        let mut violations: Vec<crate::kom::ConstraintViolation> = Vec::new();
        let heads = self.objects.scan_heads()?;
        for (hkoid, _version, _ts, state) in &heads {
            if *state == crate::kom::LifecycleState::Deleted {
                continue;
            }
            let Some(existing) = self.objects.get(hkoid)? else {
                continue;
            };
            if existing.metadata.type_name != new_schema.type_name {
                continue;
            }
            let result = self.constraint_eval.evaluate_full(
                new_schema,
                &existing.properties,
                None,
                Some(*hkoid),
                None,
            );
            for v in result.violations {
                violations.push(v);
            }
            for w in result.warnings {
                violations.push(w);
            }
        }
        Ok(violations)
    }

    /// Apply a schema migration atomically (EVO-003 apply/migrate op).
    ///
    /// Every live object of `migration.schema.type_name` not yet stamped with
    /// the target version is rewritten: property transforms are applied, the
    /// `schema_version` stamp is bumped, and the batch commits through
    /// `transact` (per-object authz via the existing write path, OCC on every
    /// head, schema + constraint validation against the NEW schema). The
    /// target schema row commits in the SAME engine batch as the data rows
    /// (SCHEMA-006: no hybrid window); on any failure only the in-memory
    /// registration is rolled back and nothing persists.
    ///
    /// Deterministic version gate: the new version must be prev+1, or — for an
    /// identical re-apply (idempotent retry) — equal to prev with the exact
    /// same schema. Warnings from constraint evaluation do not block; errors
    /// do (pre-validated on the transformed view, because `transact` skips
    /// check evaluation on empty write-sets).
    pub fn apply_schema_migration(
        &self,
        subject: &Subject,
        migration: &SchemaMigration,
    ) -> KResult<MigrationReport> {
        let new_schema = &migration.schema;
        let prev = {
            let schemas = self.schemas.read().unwrap();
            match schemas.get(&new_schema.type_name) {
                Some(s) => s.clone(),
                None => {
                    return Err(KError::InvalidSchema(format!(
                        "no schema registered for type '{}' — register an initial schema before migrating",
                        new_schema.type_name
                    )));
                }
            }
        };
        let new_version = new_schema.schema_version;
        if new_version != prev.schema_version + 1
            && (new_version != prev.schema_version || *new_schema != prev)
        {
            return Err(KError::InvalidSchema(format!(
                "schema migration for '{}' must bump version {} -> {}",
                new_schema.type_name, prev.schema_version, new_version
            )));
        }

        let heads = self.objects.scan_heads()?;
        let mut ops = Vec::new();
        let mut scanned = 0usize;
        let mut already_at_target = 0usize;
        for (hkoid, _version, _ts, state) in &heads {
            if *state == LifecycleState::Deleted {
                continue;
            }
            let Some(head) = self.objects.get(hkoid)? else {
                continue;
            };
            if head.metadata.type_name != new_schema.type_name {
                continue;
            }
            scanned += 1;
            if head.metadata.schema_version == new_version {
                already_at_target += 1;
                continue;
            }
            let mut props = head.properties.clone();
            for t in &migration.transforms {
                match t {
                    PropertyTransform::Rename { from, to } => match props.remove(from) {
                        Some(v) => {
                            props.insert(to.clone(), v);
                        }
                        None => {
                            return Err(KError::InvalidObject(format!(
                                "rename transform: property '{}' missing on {}",
                                from, hkoid
                            )));
                        }
                    },
                    PropertyTransform::SetDefault { property, value } => {
                        if !props.contains_key(property) {
                            props.insert(property.clone(), value.clone());
                        }
                    }
                }
            }
            // Pre-validate the transformed view against the new schema's
            // constraints (check/domain) — transact skips these on empty
            // write-sets, so a migration must evaluate them explicitly.
            let result =
                self.constraint_eval
                    .evaluate_full(new_schema, &props, None, Some(*hkoid), None);
            if !result.violations.is_empty() {
                return Err(KError::InvalidSchema(format!(
                    "schema migration of '{}' would violate constraints on {}: {}",
                    new_schema.type_name, hkoid, result.violations[0].message
                )));
            }
            let mut meta = head.metadata.clone();
            meta.schema_version = new_version;
            let mut req = RememberRequest::update(subject.clone(), *hkoid, meta);
            req.expected_version = Some(head.version);
            req.properties = props;
            // preserve everything the migration does not touch
            req.semantic = head.semantic.clone();
            req.relationships = head.relationships.clone();
            req.extensions = head.extensions.clone();
            req.origin = head.lifecycle.origin.clone();
            req.note = Some(format!(
                "schema migration {} -> {}",
                prev.schema_version, new_version
            ));
            ops.push(TransactionOp::new(subject.clone(), req));
        }
        let migrated = ops.len();

        // Register the target schema in-memory first so `transact` validates
        // the stamped KOs against it. The persisted row commits in the SAME
        // engine batch as the data rows (SCHEMA-006): a kill mid-migration
        // leaves valid pre OR post state, never a hybrid. On failure only
        // the in-memory registry is restored — the persisted row was never
        // written, so nothing to roll back. Transact's OCC re-check makes
        // the pre-transact scan race-safe.
        self.schemas.write().unwrap().register(new_schema.clone());
        if let Err(e) = self.transact_with_schema_row(ops, Some(new_schema)) {
            self.schemas.write().unwrap().register(prev);
            return Err(e);
        }
        Ok(MigrationReport {
            scanned,
            migrated,
            already_at_target,
        })
    }

    /// Version payload access for index maintenance (internal; bypasses ACL).
    #[doc(hidden)]
    pub fn raw_object_at(&self, koid: &KOID, commit_ts: u64) -> KResult<Option<KnowledgeObject>> {
        self.object_at(koid, commit_ts)
    }

    // ---- internal read helpers -------------------------------------------

    pub(crate) fn head_object(&self, koid: &KOID) -> KResult<Option<KnowledgeObject>> {
        self.objects.get(koid)
    }

    pub(crate) fn object_at(&self, koid: &KOID, snap_ts: u64) -> KResult<Option<KnowledgeObject>> {
        self.objects.get_at(koid, snap_ts)
    }

    pub fn scan_heads(&self) -> KResult<Vec<(KOID, u64, u64, LifecycleState)>> {
        self.objects.scan_heads()
    }

    /// `(koid, head_state)` for every object indexed under `type_name` (R9).
    /// Type-scoped similarity search iterates this instead of all heads.
    pub(crate) fn heads_of_type(&self, type_name: &str) -> KResult<Vec<(KOID, LifecycleState)>> {
        let mut out = Vec::new();
        for koid in self.repo.scan_type(type_name)? {
            let Some((_version, _ts, state)) = self.repo.get_head(&koid)? else {
                continue; // head erased under a stale index entry
            };
            out.push((koid, state));
        }
        Ok(out)
    }

    pub(crate) fn check_access(
        &self,
        subject: &Subject,
        ko: &KnowledgeObject,
        action: Action,
    ) -> KResult<()> {
        self.auth.read().unwrap().authorize(subject, ko, action)
    }

    pub(crate) fn accessible_objects(
        &self,
        subject: &Subject,
        type_name: Option<&str>,
    ) -> KResult<Vec<KnowledgeObject>> {
        // R9: with a type filter, walk the type index; without one, fall back
        // to the full head scan (no narrower scope to seek).
        let Some(tn) = type_name else {
            let mut out = Vec::new();
            for (koid, _version, _ts, state) in self.repo.scan_heads()? {
                if state == LifecycleState::Deleted {
                    continue;
                }
                let Some(ko) = self.head_object(&koid)? else {
                    continue;
                };
                if self
                    .auth
                    .read()
                    .unwrap()
                    .authorize(subject, &ko, Action::Read)
                    .is_err()
                {
                    continue;
                }
                out.push(ko);
            }
            return Ok(out);
        };
        let mut out = Vec::new();
        for koid in self.repo.scan_type(tn)? {
            let Some(ko) = self.head_object(&koid)? else {
                continue;
            };
            if ko.metadata.type_name != tn {
                continue; // stale index entry (type changed after indexing)
            }
            if ko.lifecycle.state == LifecycleState::Deleted {
                continue;
            }
            if self
                .auth
                .read()
                .unwrap()
                .authorize(subject, &ko, Action::Read)
                .is_err()
            {
                continue;
            }
            out.push(ko);
        }
        Ok(out)
    }

    // ---- verify (MRFC-0011 §6.8) ------------------------------------------

    fn refresh_auth_cache(&self) -> KResult<()> {
        self.auth.write().unwrap().refresh(&self.repo)
    }

    pub fn verify(
        &self,
        ctx: impl Into<KnowledgeContext>,
        koid: &KOID,
        action: Action,
    ) -> KResult<()> {
        let ctx = ctx.into();
        match self.head_object(koid)? {
            Some(ko) => self
                .auth
                .read()
                .unwrap()
                .authorize(&ctx.subject, &ko, action),
            None => {
                if action == Action::Write {
                    Ok(())
                } else {
                    Err(KError::NotFound(*koid))
                }
            }
        }
    }

    // ---- shared commit machinery -------------------------------------------

    /// Append one version + one KE atomically. Caller holds `pipe` lock.
    fn commit_version(
        &self,
        pipe: &mut Pipeline,
        mut ko: KnowledgeObject,
        kind: EventKind,
        origin: Origin,
        actor: &str,
        note: Option<String>,
        idem: Option<&str>,
        prev_rels: Option<&[RelationshipRef]>,
    ) -> KResult<(u64, u64)> {
        ko.validate()?;
        let commit_ts = self.hlc.now(self.clock.as_ref());
        let seq = pipe.seq + 1;
        ko.commit_ts = commit_ts;
        ko.event_refs.push(EventRef {
            seq,
            kind,
            commit_ts,
        });
        let payload = codec::encode_ko(&ko);
        let payload_hash = sha256(&payload);
        let signature = self.signing_key.map(|key| hmac_sha256(&key, &payload));
        let audit = audit_hash_of(
            pipe.audit,
            seq,
            &ko.koid,
            ko.version,
            kind,
            commit_ts,
            &payload_hash,
            signature.as_ref(),
            actor,
            note.as_deref(),
        );
        let ke = KnowledgeEvent {
            seq,
            koid: ko.koid,
            version: ko.version,
            kind,
            origin,
            actor: actor.into(),
            commit_ts,
            payload_hash,
            prev_audit_hash: pipe.audit,
            audit_hash: audit,
            signature,
            note,
        };
        let mut batch = WriteBatch::new();
        self.repo
            .put_object_version(&mut batch, &ko.koid, commit_ts, &ko);
        // Maintain relationship index: every edge in the KO gets an outbound
        // and inbound index entry keyed by (src, rel_type, dst). Idempotent
        // — re-writing the same edge is a no-op at the KV level.
        for rel in &ko.relationships {
            let (src, dst) = match rel.direction {
                Direction::Outbound => (ko.koid, rel.target),
                Direction::Inbound => (rel.target, ko.koid),
            };
            self.relationships
                .write_index(&mut batch, &src, &rel.rel_type, &dst);
        }
        // QA2-PROP-002: remove index entries for edges that were on the
        // previous head but are absent from this version (unrelate). The
        // removals land in the same batch as the commit, so the index and
        // the head can never drift — across a crash or otherwise. The diff
        // is a multiset subtraction: a removed-and-readded edge stays.
        if let Some(prev) = prev_rels {
            let mut removed: Vec<RelationshipRef> = prev.to_vec();
            for rel in &ko.relationships {
                if let Some(i) = removed.iter().position(|p| p == rel) {
                    removed.swap_remove(i);
                }
            }
            for rel in &removed {
                let (src, dst) = match rel.direction {
                    Direction::Outbound => (ko.koid, rel.target),
                    Direction::Inbound => (rel.target, ko.koid),
                };
                self.relationships
                    .delete_index(&mut batch, &src, &rel.rel_type, &dst);
            }
        }
        self.repo.put_head(
            &mut batch,
            &ko.koid,
            ko.version,
            commit_ts,
            ko.lifecycle.state,
        );
        // R9: maintain the type-scoped secondary index. Stale entries from a
        // later type change are harmless — scan_by_type re-checks the payload.
        self.repo
            .write_type_index(&mut batch, &ko.metadata.type_name, &ko.koid);
        self.repo.put_event(&mut batch, seq, &ke);
        self.repo.put_journal(&mut batch, seq, audit, commit_ts);
        if let Some(k) = idem {
            self.repo
                .put_idem(&mut batch, k, &ko.koid, ko.version, commit_ts);
        }
        self.repo.write_batch(&batch)?;
        pipe.seq = seq;
        pipe.audit = audit;
        self.broadcast(&ke);
        Ok((commit_ts, seq))
    }

    fn broadcast(&self, ke: &KnowledgeEvent) {
        self.events.lock().unwrap().broadcast(ke);
    }

    // ---- remember (MRFC-0011 §6.1) -----------------------------------------

    /// Extension keys owned by the kernel: epistemic status/history, lifecycle,
    /// Extension keys owned by the kernel: epistemic status/history, lifecycle,
    /// invalidation, evidence, derivation, confidence, verified_event,
    /// valid_to, authority, scope, and content trust. Only the semantic
    /// operations may write them — a caller supplying them to remember()
    /// would forge epistemic state (review P0-1). `valid_from` is deliberately
    /// absent: callers declare their own claim's temporal start (and may not
    /// have written `valid_to`). Public so callers can strip these keys from a
    /// read-modify-write update instead of being rejected. P2-8: this is the
    /// complete enumeration — the typed-struct migration stays deferred, but
    /// every managed key lands here.
    pub const KERNEL_MANAGED_EXTENSIONS: &[&str] = &[
        KnowledgeObject::EXT_EPISTEMIC_STATUS,
        KnowledgeObject::EXT_EPISTEMIC_HISTORY,
        KnowledgeObject::EXT_LIFECYCLE_HISTORY,
        KnowledgeObject::EXT_INVALIDATION,
        KnowledgeObject::EXT_EVIDENCE,
        KnowledgeObject::EXT_DERIVATION,
        KnowledgeObject::EXT_CONFIDENCE,
        KnowledgeObject::EXT_VERIFIED_EVENT,
        KnowledgeObject::EXT_VALID_TO,
        KnowledgeObject::EXT_CONTENT_TRUST,
        "authority",
        "scope",
    ];

    /// Public entry point: the epistemic-metadata boundary. Kernel-managed
    /// extension keys are rejected here so no external caller can mint a
    /// Verified claim, a forged authority, or a fabricated evidence trail.
    pub fn remember(&self, req: RememberRequest) -> KResult<Remembered> {
        for key in Self::KERNEL_MANAGED_EXTENSIONS {
            if req.extensions.contains_key(*key) {
                return Err(KError::InvalidObject(format!(
                    "extension '{key}' is kernel-managed — set it via the semantic \
                     operations (assert/verify/contradict/supersede/merge/invalidate/\
                     derive), not remember()"
                )));
            }
        }
        self.remember_trusted(req)
    }

    /// Declarative retention (G13 / RET-CHAT-001): commit through the normal
    /// write path with an automatic expiry horizon. The kernel computes
    /// `valid_to = clock_now() + retention_ms` from its own clock, so callers
    /// can never forge the stamp (P0-1: EXT_VALID_TO stays kernel-managed).
    /// Updating an existing KO refreshes the horizon — the stamped value wins
    /// the update carry-forward. Expired KOs stay readable via get/lineage
    /// (RET-CHAT-003) but drop out of default-time retrieval.
    pub fn remember_retained(
        &self,
        mut req: RememberRequest,
        retention_ms: u64,
    ) -> KResult<Remembered> {
        let now = self.clock_now();
        // Same checked arithmetic as record_experience (Review P1-6): a
        // hostile u64::MAX retention must be rejected, not wrapped.
        let valid_to = now
            .checked_add(retention_ms)
            .filter(|v| *v <= i64::MAX as u64)
            .ok_or_else(|| {
                KError::InvalidObject(
                    "retention pushes valid_to past the representable epoch bound".into(),
                )
            })?;
        // The effective interval must never invert (P1-1), on create too:
        // remember_locked's inversion check only covers the update path.
        if let Some(f) = req
            .extensions
            .get(KnowledgeObject::EXT_VALID_FROM)
            .and_then(|v| match v {
                Value::Int(i) if *i >= 0 => Some(*i as u64),
                _ => None,
            })
        {
            if f > valid_to {
                return Err(KError::InvalidObject(format!(
                    "valid interval must satisfy valid_from <= valid_to (got {f} > {valid_to})"
                )));
            }
        }
        req.extensions.insert(
            KnowledgeObject::EXT_VALID_TO.into(),
            Value::Int(valid_to as i64),
        );
        self.remember_trusted(req)
    }

    /// remember() without the managed-extension guard — for the semantic
    /// operations (K4) that construct extension maps with kernel-owned keys
    /// and commit through the same locked path.
    fn remember_trusted(&self, req: RememberRequest) -> KResult<Remembered> {
        let mut pipe = self.pipe.lock().unwrap();
        self.remember_locked(&mut pipe, &req)
    }

    /// remember() with the pipe lock already held — internal to composite
    /// knowledge ops (K4) so multi-KO operations commit under one lock.
    pub(crate) fn remember_locked(
        &self,
        pipe: &mut Pipeline,
        req: &RememberRequest,
    ) -> KResult<Remembered> {
        if let Some(k) = &req.idempotency_key {
            if let Some((koid, version, commit_ts)) = self.repo.get_idem(k)? {
                return Ok(Remembered {
                    koid,
                    version,
                    commit_ts,
                }); // exact-once replay
            }
        }
        let koid = match req.koid {
            Some(k) => k,
            None => self.idgen.lock().unwrap().next(self.clock.millis()),
        };
        let head = self.head_object(&koid)?;
        if head.is_none() && req.koid.is_some() && req.expected_version.is_none() {
            // explicit target without an insert guard => update on missing object
            return Err(KError::NotFound(koid));
        }
        let cur_v = head.as_ref().map(|h| h.version).unwrap_or(0);
        let expected = req.expected_version.unwrap_or(cur_v);
        if expected != cur_v {
            return Err(KError::VersionConflict {
                koid,
                expected,
                found: cur_v,
            });
        }
        let creating = head.is_none();
        // v0.3 K1: prepare extensions so every committed KO carries explicit
        // epistemic metadata, and updates never silently drop it.
        let mut extensions = req.extensions.clone();
        if creating {
            // Stamp defaults for writes that declare none — the kernel, not
            // the caller, owns the epistemic baseline.
            if !extensions.contains_key(KnowledgeObject::EXT_EPISTEMIC_STATUS) {
                extensions.insert(
                    KnowledgeObject::EXT_EPISTEMIC_STATUS.into(),
                    Value::Text(EpistemicStatus::for_origin(&req.origin).as_str().into()),
                );
            }
            if !extensions.contains_key("authority") {
                extensions.insert(
                    "authority".into(),
                    Value::Text(Authority::for_origin(&req.origin).as_str().into()),
                );
            }
            if !extensions.contains_key("scope") {
                extensions.insert(
                    "scope".into(),
                    Value::Text(Scope::for_origin(&req.origin).as_str().into()),
                );
            }
        } else {
            let head = head.as_ref().unwrap();
            // Carry forward epistemic/provenance metadata the caller did not
            // restate — updates used to replace the whole extension map.
            for key in [
                KnowledgeObject::EXT_EPISTEMIC_STATUS,
                KnowledgeObject::EXT_EPISTEMIC_HISTORY,
                KnowledgeObject::EXT_EVIDENCE,
                KnowledgeObject::EXT_LIFECYCLE_HISTORY,
                KnowledgeObject::EXT_CONTENT_TRUST,
                KnowledgeObject::EXT_VALID_FROM,
                KnowledgeObject::EXT_VALID_TO,
                KnowledgeObject::EXT_DERIVATION,
                KnowledgeObject::EXT_CONFIDENCE,
                "authority",
                "scope",
            ] {
                if !extensions.contains_key(key) {
                    if let Some(v) = head.extensions.get(key) {
                        extensions.insert(key.into(), v.clone());
                    }
                }
            }
            // Authority is monotonic-up on ordinary updates; a downgrade
            // requires an admin (explicit escalation path).
            let parse_rank = |v: &Value| match v {
                Value::Text(s) => Authority::from_str(s).map(|a| a.rank()),
                _ => None,
            };
            if let (Some(head_a), Some(req_a)) = (
                head.extensions.get("authority").and_then(parse_rank),
                extensions.get("authority").and_then(parse_rank),
            ) {
                if req_a < head_a && !req.context.subject.is_admin() {
                    return Err(KError::InvalidObject(format!(
                        "authority downgrade ({} -> {}) requires admin",
                        head_a, req_a
                    )));
                }
            }
            // MRFC-0060 Phase R12: source_artifact/revision are immutable once
            // written; evidence is append-only — the head's list must be a
            // prefix of the request's (entries never change or vanish).
            // P1-1 (review): the effective interval must never invert —
            // valid_from <= valid_to, checked here so no caller or internal
            // op can commit a KO whose interval runs backwards. Equality is
            // legal: a claim superseded/invalidated at its own assertion
            // instant (or a future fact closed before it became valid) has a
            // zero-duration interval and is valid nowhere — by design.
            let int_of = |v: &Value| match v {
                Value::Int(i) if *i >= 0 => Some(*i as u64),
                _ => None,
            };
            if let (Some(f), Some(t)) = (
                extensions
                    .get(KnowledgeObject::EXT_VALID_FROM)
                    .and_then(int_of),
                extensions
                    .get(KnowledgeObject::EXT_VALID_TO)
                    .and_then(int_of),
            ) {
                if f > t {
                    return Err(KError::InvalidObject(format!(
                        "valid interval must satisfy valid_from <= valid_to (got {f} > {t})"
                    )));
                }
            }
            // MRFC-0060 Phase R12: source_artifact/revision are immutable once
            // written; evidence is append-only — the head's list must be a
            // prefix of the request's (entries never change or vanish).
            for key in &["source_artifact", "revision"] {
                if head.extensions.contains_key(*key)
                    && extensions.get(*key) != head.extensions.get(*key)
                {
                    return Err(KError::InvalidObject(format!(
                        "provenance field '{}' is immutable — cannot be changed after creation",
                        key
                    )));
                }
            }
            match head.extensions.get(KnowledgeObject::EXT_EVIDENCE) {
                Some(Value::List(head_ev)) => match extensions.get(KnowledgeObject::EXT_EVIDENCE) {
                    Some(Value::List(req_ev))
                        if req_ev.len() >= head_ev.len()
                            && req_ev[..head_ev.len()] == head_ev[..] => {}
                    _ => {
                        return Err(KError::InvalidObject(
                                "provenance field 'evidence' is append-only — existing entries cannot be changed or removed"
                                    .into(),
                            ));
                    }
                },
                // Legacy non-list evidence keeps the strict equality rule.
                Some(other) if extensions.get(KnowledgeObject::EXT_EVIDENCE) == Some(other) => {}
                Some(_) => {
                    return Err(KError::InvalidObject(
                        "provenance field 'evidence' is immutable — cannot be changed after creation"
                            .into(),
                    ));
                }
                None => {}
            }
        }
        // MRFC-0060 Phase C6: compute write-set for incremental constraint evaluation.
        let write_set: Option<HashSet<String>> = if creating {
            None // evaluate all constraints for creates
        } else {
            let head_props = &head.as_ref().unwrap().properties;
            let mut changed = HashSet::new();
            for k in req.properties.keys() {
                if head_props.get(k) != req.properties.get(k) {
                    changed.insert(k.clone());
                }
            }
            for k in head_props.keys() {
                if !req.properties.contains_key(k) {
                    changed.insert(k.clone());
                }
            }
            Some(changed)
        };
        let security = if creating {
            let s = req.security.clone().unwrap_or_else(|| SecurityDescriptor {
                owner: req.context.subject.name.clone(),
                acl: vec![],
                classification: None,
            });
            if s.owner != req.context.subject.name && !req.context.subject.is_admin() {
                return Err(KError::AccessDenied {
                    subject: req.context.subject.name.clone(),
                    action: Action::Write,
                    koid,
                });
            }
            s
        } else {
            let h = head.as_ref().unwrap();
            self.auth
                .read()
                .unwrap()
                .authorize(&req.context.subject, h, Action::Write)?;
            req.security.clone().unwrap_or_else(|| h.security.clone())
        };
        if req.referential_policy == ReferentialPolicy::Strict {
            for rel in &req.relationships {
                if self.head_object(&rel.target)?.is_none() {
                    return Err(KError::InvalidObject(format!(
                        "relationship target {} does not exist under strict referential policy",
                        rel.target
                    )));
                }
            }
        }
        let mut ko = KnowledgeObject {
            koid,
            version: cur_v + 1,
            commit_ts: 0,
            metadata: req.metadata.clone(),
            properties: req.properties.clone(),
            semantic: req.semantic.clone(),
            relationships: if creating {
                req.relationships.clone()
            } else {
                // Kernel-managed edges (written by derive/supersede/contradict)
                // survive updates the caller did not restate — dropping them
                // would break lineage BFS (DERIVED_FROM), supersession
                // traversal, and conflict resolution. Symmetric with the
                // extension carry-forward above. Caller-restated edges are
                // replaced wholesale as before (remember() semantics).
                let mut rels = req.relationships.clone();
                let h = head.as_ref().unwrap();
                for hr in &h.relationships {
                    if (hr.rel_type == SUPERSEDES
                        || hr.rel_type == DERIVED_FROM
                        || hr.rel_type == CONTRADICTS)
                        && !rels.contains(hr)
                    {
                        rels.push(hr.clone());
                    }
                }
                rels
            },
            event_refs: head
                .as_ref()
                .map(|h| h.event_refs.clone())
                // justified: no prior head → empty event_refs (new KO)
                .unwrap_or_default(),
            security,
            lifecycle: head
                .as_ref()
                .map(|h| h.lifecycle.clone())
                .unwrap_or(Lifecycle {
                    state: LifecycleState::Draft,
                    origin: req.origin.clone(),
                }),
            extensions,
        };
        {
            let schemas = self.schemas.read().unwrap();
            schemas.validate(&ko, self.constraint_caps.not_null)?;
            // MRFC-0060 Phase C4/C5/C7: domain + check constraint evaluation (skip if backend native)
            if !self.constraint_caps.check {
                if let Some(schema) = schemas.get(&ko.metadata.type_name) {
                    self.constraint_eval
                        .evaluate_full(
                            schema,
                            &ko.properties,
                            write_set.as_ref(),
                            Some(ko.koid),
                            ko.semantic.as_ref().and_then(|s| s.source.as_deref()),
                        )
                        .into_kresult()?;
                }
            }
        }
        // MRFC-0060 Phase C2/C7: uniqueness check — skip if backend enforces unique natively
        if !self.constraint_caps.unique {
            let objects = &self.objects;
            self.schemas.read().unwrap().check_uniqueness(
                &ko,
                |scope, tenant, type_name, pairs, exclude_koid| {
                    uniqueness_conflict(objects, scope, tenant, type_name, pairs, exclude_koid)
                },
                false, // remember() checks all constraints including deferred
                write_set.as_ref(),
            )?;
        }
        // MRFC-0060 Phase C3: ontology relationship validation.
        if req.referential_policy == ReferentialPolicy::Enforced {
            let ont = self.ontologies.read().unwrap();
            let mut req_rel_counts: HashMap<String, u32> = HashMap::new();
            for rel in &req.relationships {
                *req_rel_counts.entry(rel.rel_type.clone()).or_insert(0) += 1;
            }
            let mut checked_outbound: HashSet<String> = HashSet::new();
            for rel in &req.relationships {
                if let Some(rel_def) = ont.definition().relationships.get(&rel.rel_type) {
                    // Domain: source type must match (with subclass support).
                    if let Some(ref domain) = rel_def.domain {
                        if req.metadata.type_name != *domain
                            && !ont.is_subclass_of(&req.metadata.type_name, domain)
                        {
                            return Err(KError::InvalidObject(format!(
                                "relationship '{}' domain mismatch: '{}' is not a '{}'",
                                rel.rel_type, req.metadata.type_name, domain
                            )));
                        }
                    }
                    // Range: target type must match (with subclass support).
                    if let Some(ref range) = rel_def.range {
                        if let Some(target_ko) = self.head_object(&rel.target)? {
                            if target_ko.metadata.type_name != *range
                                && !ont.is_subclass_of(&target_ko.metadata.type_name, range)
                            {
                                return Err(KError::InvalidObject(format!(
                                    "relationship '{}' range mismatch: target '{}' is not a '{}'",
                                    rel.rel_type, target_ko.metadata.type_name, range
                                )));
                            }
                        }
                    }
                    let is_one_to_one = rel_def.cardinality == Some(Cardinality::OneToOne);
                    let is_one_to_many = rel_def.cardinality == Some(Cardinality::OneToMany);

                    // 1:1 and 1:N: each target can only be referenced once for
                    // this relationship type (inbound exclusivity).
                    if is_one_to_one || is_one_to_many {
                        let inbound = self
                            .relationships
                            .inbound(&rel.target, Some(&rel.rel_type))?;
                        if inbound.iter().any(|(_, src)| src != &koid) {
                            return Err(KError::InvalidObject(format!(
                                "cardinality violated: target {} already has '{}' from another source",
                                rel.target, rel.rel_type
                            )));
                        }
                    }

                    // Outbound-side checks: run once per unique rel_type.
                    if checked_outbound.insert(rel.rel_type.clone()) {
                        let req_count = *req_rel_counts.get(&rel.rel_type).unwrap_or(&0);
                        // 1:1: source can only emit one relationship of this type.
                        if is_one_to_one && req_count > 1 {
                            return Err(KError::InvalidObject(format!(
                                "1:1 cardinality violated: {} '{}' relationships in request (max 1)",
                                req_count, rel.rel_type
                            )));
                        }
                        // User-defined max_count on outbound relationships.
                        if let Some(max) = rel_def.max_count {
                            if req_count > max {
                                return Err(KError::InvalidObject(format!(
                                    "max_count cardinality violated: {} '{}' relationships (max {})",
                                    req_count, rel.rel_type, max
                                )));
                            }
                        }
                    }
                }
            }
        }
        // Enforce tenant quota on creation (Phase 5 multi-tenancy).
        if creating {
            self.tenants.check_create(req.context.tenant.as_deref())?;
        }
        // Field-level encryption (MRFC-0020 Phase 3).
        if let Some(ref fc) = self.field_crypto {
            if let Some(policy) = self
                .encryption_policies
                .read()
                .unwrap()
                .get(&req.metadata.type_name)
            {
                let tenant = req.context.tenant.as_deref().unwrap_or("default");
                let encrypted = fc
                    .encrypt_fields(tenant, &req.metadata.type_name, &mut ko.properties, policy)
                    .map_err(|e| KError::Store(format!("field encrypt: {}", e)))?;
                if encrypted > 0 {
                    // Persist wrapped DEKs BEFORE the object commit: a crash in
                    // between leaves an orphan DEK record (harmless); the
                    // reverse would leave field ciphertext with no recoverable
                    // key. ponytail: unconditional rewrite (tens of bytes);
                    // dirty-tracking if this path gets hot.
                    let mut batch = WriteBatch::new();
                    batch.put(
                        DEKS_STORAGE_KEY.to_vec(),
                        Envelope::encode_wrapped_deks(&fc.wrapped_deks()),
                    );
                    self.store
                        .write_batch(&batch)
                        .map_err(|e| KError::Store(format!("DEK persist: {}", e)))?;
                }
            }
        }
        // claims via Class B keep ClaimAsserted kind
        let kind = match (&req.origin, creating) {
            (Origin::Reason, _) | (Origin::SemanticEnrichment, _) => EventKind::ClaimAsserted,
            (_, true) => EventKind::Created,
            (_, false) => EventKind::Updated,
        };
        let is_auth_meta =
            req.metadata.type_name == ROLE_TYPE || req.metadata.type_name == POLICY_TYPE;
        let (commit_ts, _seq) = self.commit_version(
            pipe,
            ko,
            kind,
            req.origin.clone(),
            &req.context.subject.name,
            req.note.clone(),
            req.idempotency_key.as_deref(),
            head.as_ref().map(|h| h.relationships.as_slice()),
        )?;
        if is_auth_meta {
            self.refresh_auth_cache()?;
        }
        Ok(Remembered {
            koid,
            version: cur_v + 1,
            commit_ts,
        })
    }

    // ---- transact (multi-object atomic commit) ------------------------------

    /// Atomically commit multiple remember operations as one batch.
    ///
    /// Guarantees:
    /// - all-or-nothing persistence (single StorageEngine::write_batch);
    /// - OCC checks use the snapshot taken before any write;
    /// - strict referential integrity resolves targets created within the batch;
    /// - gapless, monotone journal sequence for the whole batch.
    ///
    /// Idempotency keys inside transaction requests are not supported: a batch
    /// is already atomic and the caller can use an external idempotency token.
    pub fn transact(&self, ops: Vec<TransactionOp>) -> KResult<Vec<Remembered>> {
        self.transact_with_schema_row(ops, None)
    }

    /// Internal: `transact` with an optional schema row folded into the same
    /// commit batch (SCHEMA-006). The schema row must become durable in the
    /// SAME engine batch as the data rows it validates — two batches leave a
    /// kill window where the new schema row coexists with old-version data.
    fn transact_with_schema_row(
        &self,
        ops: Vec<TransactionOp>,
        schema_row: Option<&Schema>,
    ) -> KResult<Vec<Remembered>> {
        if ops.is_empty() {
            let Some(schema) = schema_row else {
                return Ok(Vec::new());
            };
            // Idempotent re-apply with nothing left to migrate: persist the
            // schema row alone so the persisted registry never lags the
            // data stamps (all heads already at the target version).
            let mut batch = WriteBatch::new();
            let bytes = crate::knowledge::codec::encode_schema(schema);
            self.repo
                .put_schema_row(&mut batch, &schema.type_name, &bytes);
            self.repo.write_batch(&batch)?;
            return Ok(Vec::new());
        }
        let mut pipe = self.pipe.lock().unwrap();

        // Phase 1: resolve KOIDs and heads (snapshot before any write).
        struct Resolved {
            koid: KOID,
            head: Option<KnowledgeObject>,
            cur_v: u64,
            creating: bool,
            op: TransactionOp,
        }
        let mut resolved = Vec::with_capacity(ops.len());
        for op in ops {
            if op.request.idempotency_key.is_some() {
                return Err(KError::UnsupportedOperation(
                    "idempotency keys are not supported inside transactions".into(),
                ));
            }
            let koid = match op.request.koid {
                Some(k) => k,
                None => self.idgen.lock().unwrap().next(self.clock.millis()),
            };
            let head = self.head_object(&koid)?;
            if head.is_none() && op.request.koid.is_some() && op.request.expected_version.is_none()
            {
                return Err(KError::NotFound(koid));
            }
            let cur_v = head.as_ref().map(|h| h.version).unwrap_or(0);
            let expected = op.request.expected_version.unwrap_or(cur_v);
            if expected != cur_v {
                return Err(KError::VersionConflict {
                    koid,
                    expected,
                    found: cur_v,
                });
            }
            resolved.push(Resolved {
                koid,
                head,
                cur_v,
                creating: cur_v == 0,
                op,
            });
        }

        // Detect duplicate KOIDs in the batch -> deterministic conflict.
        let mut seen = HashSet::new();
        for r in &resolved {
            if !seen.insert(r.koid) {
                return Err(KError::VersionConflict {
                    koid: r.koid,
                    expected: r.cur_v + 1,
                    found: r.cur_v + 1,
                });
            }
        }

        // Set of KOIDs that will exist after the batch (for referential checks).
        let new_koids: HashSet<KOID> = resolved.iter().map(|r| r.koid).collect();

        // MRFC-0060 Phase C3: batch tracking for range + cardinality checks.
        let batch_types: HashMap<KOID, String> = resolved
            .iter()
            .map(|r| (r.koid, r.op.request.metadata.type_name.clone()))
            .collect();
        let mut batch_inbound: HashMap<(KOID, String), HashSet<KOID>> = HashMap::new();
        let mut txn_state = crate::lifecycle::constraint::TransactionConstraintState::new();

        // Phase 2: authorize, validate referential integrity, build object versions.
        let mut pending = Vec::with_capacity(resolved.len());
        for r in &resolved {
            let req = &r.op.request;
            let security = if r.creating {
                let s = req.security.clone().unwrap_or_else(|| SecurityDescriptor {
                    owner: r.op.context.subject.name.clone(),
                    acl: vec![],
                    classification: None,
                });
                if s.owner != r.op.context.subject.name && !r.op.context.subject.is_admin() {
                    return Err(KError::AccessDenied {
                        subject: r.op.context.subject.name.clone(),
                        action: Action::Write,
                        koid: r.koid,
                    });
                }
                s
            } else {
                let h = r.head.as_ref().unwrap();
                self.auth
                    .read()
                    .unwrap()
                    .authorize(&r.op.context.subject, h, Action::Write)?;
                req.security.clone().unwrap_or_else(|| h.security.clone())
            };
            if req.referential_policy == ReferentialPolicy::Strict {
                for rel in &req.relationships {
                    if !new_koids.contains(&rel.target) && self.head_object(&rel.target)?.is_none()
                    {
                        return Err(KError::InvalidObject(format!(
                            "relationship target {} does not exist under strict referential policy",
                            rel.target
                        )));
                    }
                }
            }
            let ko = KnowledgeObject {
                koid: r.koid,
                version: r.cur_v + 1,
                commit_ts: 0,
                metadata: req.metadata.clone(),
                properties: req.properties.clone(),
                semantic: req.semantic.clone(),
                relationships: req.relationships.clone(),
                event_refs: r
                    .head
                    .as_ref()
                    .map(|h| h.event_refs.clone())
                    // justified: no prior head → empty event_refs (new KO)
                    .unwrap_or_default(),
                security,
                lifecycle: r
                    .head
                    .as_ref()
                    .map(|h| h.lifecycle.clone())
                    .unwrap_or(Lifecycle {
                        state: LifecycleState::Draft,
                        origin: req.origin.clone(),
                    }),
                extensions: req.extensions.clone(),
            };
            // MRFC-0060 Phase C6: write-set for incremental constraint evaluation.
            let tx_write_set: Option<HashSet<String>> = if r.creating {
                None
            } else {
                let head_props = &r.head.as_ref().unwrap().properties;
                let mut changed = HashSet::new();
                for k in req.properties.keys() {
                    if head_props.get(k) != req.properties.get(k) {
                        changed.insert(k.clone());
                    }
                }
                for k in head_props.keys() {
                    if !req.properties.contains_key(k) {
                        changed.insert(k.clone());
                    }
                }
                Some(changed)
            };
            {
                let schemas = self.schemas.read().unwrap();
                schemas.validate(&ko, self.constraint_caps.not_null)?;
                // MRFC-0060 Phase C7: skip check constraint eval if backend native
                if !self.constraint_caps.check {
                    if let Some(schema) = schemas.get(&ko.metadata.type_name) {
                        self.constraint_eval.evaluate(
                            schema,
                            &ko.properties,
                            tx_write_set.as_ref(),
                        )?;
                    }
                }
            }
            // MRFC-0060 Phase C5: immediate uniqueness check + deferred constraint collection.
            {
                let schemas = self.schemas.read().unwrap();
                let objects = &self.objects;
                // MRFC-0060 Phase C7: skip immediate uniqueness check if backend native.
                // Deferred unique constraints are always evaluated in-kernel.
                if !self.constraint_caps.unique {
                    schemas.check_uniqueness(
                        &ko,
                        |scope, tenant, type_name, pairs, exclude_koid| {
                            uniqueness_conflict(
                                objects,
                                scope,
                                tenant,
                                type_name,
                                pairs,
                                exclude_koid,
                            )
                        },
                        true, // skip deferred — collected below
                        tx_write_set.as_ref(),
                    )?;
                }
                // Deferred constraints are never pushed down (StorageEngine has no txn handles).
                for (ci, pairs, scope) in schemas.collect_deferred_unique(&ko) {
                    txn_state.record_unique(
                        &ko.metadata.type_name,
                        ci,
                        pairs,
                        ko.koid,
                        scope,
                        ko.metadata.tenant.clone(),
                    );
                }
                if let Some(schema) = schemas.get(&ko.metadata.type_name) {
                    for (ci, cc) in schema.check_constraints.iter().enumerate() {
                        if cc.timing == ConstraintTiming::Deferred {
                            // C6: skip deferred checks unaffected by write-set
                            if !crate::lifecycle::constraint::check_affected_by_write_set(
                                cc,
                                tx_write_set.as_ref(),
                            ) {
                                continue;
                            }
                            txn_state.record_check(
                                &ko.metadata.type_name,
                                ci,
                                ko.koid,
                                ko.properties.clone(),
                            );
                        }
                    }
                }
            }
            // MRFC-0060 Phase C3: ontology relationship validation.
            if req.referential_policy == ReferentialPolicy::Enforced {
                let ont = self.ontologies.read().unwrap();
                let mut req_rel_counts: HashMap<String, u32> = HashMap::new();
                for rel in &req.relationships {
                    *req_rel_counts.entry(rel.rel_type.clone()).or_insert(0) += 1;
                }
                let mut checked_outbound: HashSet<String> = HashSet::new();
                for rel in &req.relationships {
                    if let Some(rel_def) = ont.definition().relationships.get(&rel.rel_type) {
                        // Domain check.
                        if let Some(ref domain) = rel_def.domain {
                            if req.metadata.type_name != *domain
                                && !ont.is_subclass_of(&req.metadata.type_name, domain)
                            {
                                return Err(KError::InvalidObject(format!(
                                    "relationship '{}' domain mismatch: '{}' is not a '{}'",
                                    rel.rel_type, req.metadata.type_name, domain
                                )));
                            }
                        }
                        // Range check: resolve within-batch or from storage.
                        if let Some(ref range) = rel_def.range {
                            let target_type = if new_koids.contains(&rel.target) {
                                batch_types.get(&rel.target).cloned()
                            } else {
                                self.head_object(&rel.target)?
                                    .map(|ko| ko.metadata.type_name)
                            };
                            if let Some(ref tt) = target_type {
                                if tt != range && !ont.is_subclass_of(tt, range) {
                                    return Err(KError::InvalidObject(format!(
                                        "relationship '{}' range mismatch: target '{}' is not a '{}'",
                                        rel.rel_type, tt, range
                                    )));
                                }
                            }
                        }
                        let is_one_to_one = rel_def.cardinality == Some(Cardinality::OneToOne);
                        let is_one_to_many = rel_def.cardinality == Some(Cardinality::OneToMany);

                        // 1:1 and 1:N: inbound exclusivity across storage + batch.
                        if is_one_to_one || is_one_to_many {
                            let inbound = self
                                .relationships
                                .inbound(&rel.target, Some(&rel.rel_type))?;
                            let stored_conflict = inbound.iter().any(|(_, src)| src != &r.koid);
                            let batch_conflict = batch_inbound
                                .get(&(rel.target, rel.rel_type.clone()))
                                .map(|sources| sources.iter().any(|s| s != &r.koid))
                                .unwrap_or(false);
                            if stored_conflict || batch_conflict {
                                return Err(KError::InvalidObject(format!(
                                    "cardinality violated: target {} already has '{}' from another source",
                                    rel.target, rel.rel_type
                                )));
                            }
                        }

                        // Outbound-side checks: run once per unique rel_type per op.
                        if checked_outbound.insert(rel.rel_type.clone()) {
                            let req_count = *req_rel_counts.get(&rel.rel_type).unwrap_or(&0);
                            if is_one_to_one && req_count > 1 {
                                return Err(KError::InvalidObject(format!(
                                    "1:1 cardinality violated: {} '{}' relationships in request (max 1)",
                                    req_count, rel.rel_type
                                )));
                            }
                            if let Some(max) = rel_def.max_count {
                                if req_count > max {
                                    return Err(KError::InvalidObject(format!(
                                        "max_count cardinality violated: {} '{}' relationships (max {})",
                                        req_count, rel.rel_type, max
                                    )));
                                }
                            }
                        }

                        // Track this relationship for intra-batch cardinality.
                        batch_inbound
                            .entry((rel.target, rel.rel_type.clone()))
                            .or_default()
                            .insert(r.koid);
                    }
                }
            }
            let kind = match (&req.origin, r.creating) {
                (Origin::Reason, _) | (Origin::SemanticEnrichment, _) => EventKind::ClaimAsserted,
                (_, true) => EventKind::Created,
                (_, false) => EventKind::Updated,
            };
            pending.push((
                r.koid,
                r.cur_v,
                ko,
                kind,
                req.origin.clone(),
                r.op.context.subject.name.clone(),
                req.note.clone(),
                // prev rels for the phase-3 index diff (unrelate sweep) —
                // mirror of commit_version's QA2-PROP-002 invariant.
                r.head
                    .as_ref()
                    .map(|h| h.relationships.clone())
                    .unwrap_or_default(),
            ));
        }

        // MRFC-0060 Phase C5: deferred constraint evaluation before commit.
        if !txn_state.is_empty() {
            let schemas = self.schemas.read().unwrap();
            let objects = &self.objects;
            let result = self.constraint_eval.evaluate_deferred(
                &txn_state,
                &schemas,
                |scope, tenant, type_name, pairs, exclude_koid| {
                    uniqueness_conflict(objects, scope, tenant, type_name, pairs, exclude_koid)
                },
                0, // pre-commit — timestamp assigned in Phase 3
            );
            result.into_kresult()?;
        }

        // Phase 3: assign one commit timestamp and sequential event sequence numbers.
        let commit_ts = self.hlc.now(self.clock.as_ref());
        let mut batch = WriteBatch::new();
        let mut events: Vec<KnowledgeEvent> = Vec::with_capacity(pending.len());
        let mut results = Vec::with_capacity(pending.len());
        let mut prev_audit = pipe.audit;
        let start_seq = pipe.seq;
        for (idx, (koid, cur_v, mut ko, kind, origin, actor, note, prev_rels)) in
            pending.into_iter().enumerate()
        {
            let seq = start_seq + idx as u64 + 1;
            ko.commit_ts = commit_ts;
            ko.event_refs.push(EventRef {
                seq,
                kind,
                commit_ts,
            });
            ko.validate()?;
            let payload = codec::encode_ko(&ko);
            let payload_hash = sha256(&payload);
            let signature = self.signing_key.map(|key| hmac_sha256(&key, &payload));
            let audit = audit_hash_of(
                prev_audit,
                seq,
                &koid,
                ko.version,
                kind,
                commit_ts,
                &payload_hash,
                signature.as_ref(),
                &actor,
                note.as_deref(),
            );
            let ke = KnowledgeEvent {
                seq,
                koid,
                version: ko.version,
                kind,
                origin,
                actor,
                commit_ts,
                payload_hash,
                prev_audit_hash: prev_audit,
                audit_hash: audit,
                signature,
                note,
            };
            self.repo
                .put_object_version(&mut batch, &koid, commit_ts, &ko);
            self.repo
                .put_head(&mut batch, &koid, ko.version, commit_ts, ko.lifecycle.state);
            // Relationship + type index maintenance — the same invariant
            // commit_version upholds (QA2-PROP-002): index rows are atoms of
            // the same batch as the version row, so the index can never
            // drift from the head, across a crash or otherwise.
            for rel in &ko.relationships {
                let (src, dst) = match rel.direction {
                    Direction::Outbound => (koid, rel.target),
                    Direction::Inbound => (rel.target, koid),
                };
                self.relationships
                    .write_index(&mut batch, &src, &rel.rel_type, &dst);
            }
            let mut removed: Vec<RelationshipRef> = prev_rels.to_vec();
            for rel in &ko.relationships {
                if let Some(i) = removed.iter().position(|p| p == rel) {
                    removed.swap_remove(i);
                }
            }
            for rel in &removed {
                let (src, dst) = match rel.direction {
                    Direction::Outbound => (koid, rel.target),
                    Direction::Inbound => (rel.target, koid),
                };
                self.relationships
                    .delete_index(&mut batch, &src, &rel.rel_type, &dst);
            }
            self.repo
                .write_type_index(&mut batch, &ko.metadata.type_name, &koid);
            self.repo.put_event(&mut batch, seq, &ke);
            events.push(ke);
            prev_audit = audit;
            results.push(Remembered {
                koid,
                version: cur_v + 1,
                commit_ts,
            });
        }
        let final_seq = start_seq + events.len() as u64;
        self.repo
            .put_journal(&mut batch, final_seq, prev_audit, commit_ts);
        if let Some(schema) = schema_row {
            let bytes = crate::knowledge::codec::encode_schema(schema);
            self.repo
                .put_schema_row(&mut batch, &schema.type_name, &bytes);
        }
        self.repo.write_batch(&batch)?;
        pipe.seq = final_seq;
        pipe.audit = prev_audit;
        for ke in events {
            self.broadcast(&ke);
        }
        drop(pipe);
        if results.iter().any(|r| {
            let h = self.head_object(&r.koid).ok().flatten();
            h.map(|h| h.metadata.type_name == ROLE_TYPE || h.metadata.type_name == POLICY_TYPE)
                .unwrap_or(false)
        }) {
            self.refresh_auth_cache()?;
        }
        Ok(results)
    }

    // ---- evolve (MRFC-0011 §6.3) -------------------------------------------

    pub fn evolve(
        &self,
        ctx: impl Into<KnowledgeContext>,
        koid: &KOID,
        to: LifecycleState,
        origin: Origin,
        expected_version: Option<u64>,
        note: Option<String>,
    ) -> KResult<Evolved> {
        let ctx = ctx.into();
        let mut pipe = self.pipe.lock().unwrap();
        let head = self.head_object(koid)?.ok_or(KError::NotFound(*koid))?;
        self.auth
            .read()
            .unwrap()
            .authorize(&ctx.subject, &head, Action::Evolve)?;
        let from = head.lifecycle.state;
        if !from.can_transition(to) {
            return Err(KError::InvalidState { from, to });
        }
        let cur_v = head.version;
        let expected = expected_version.unwrap_or(cur_v);
        if expected != cur_v {
            return Err(KError::VersionConflict {
                koid: *koid,
                expected,
                found: cur_v,
            });
        }
        let mut ko = head.clone();
        ko.version = cur_v + 1;
        ko.lifecycle = Lifecycle {
            state: to,
            origin: origin.clone(),
        };
        // v0.3 K1: lifecycle transitions create evidence — append to the
        // append-only history before commit.
        ko.push_lifecycle_history(
            from,
            to,
            self.clock.millis(),
            &ctx.subject.name,
            note.as_deref(),
        );
        let (commit_ts, _seq) = self.commit_version(
            &mut pipe,
            ko,
            EventKind::LifecycleChanged,
            origin,
            &ctx.subject.name,
            note,
            None,
            Some(&head.relationships),
        )?;
        Ok(Evolved {
            koid: *koid,
            version: cur_v + 1,
            commit_ts,
            state: to,
        })
    }

    // ---- epistemic transitions (v0.3 K1) -----------------------------------

    /// Move a KO's epistemic status under the constrained transition table.
    /// Appends to the append-only history extension, bumps the version, and
    /// lands an `EpistemicChanged` event in the audit chain. Transitions
    /// create evidence: the history entry records from/to, wall-clock,
    /// actor, and reason.
    ///
    /// Explicitly privileged epistemic transition (review P0-2). The
    /// `admin_` prefix is the contract: this bypasses the semantic ops'
    /// evidence validation, confidence accounting, and dependent sweeps, and
    /// is therefore reserved for an embedder's own admin surface (or
    /// multi-KO composite ops inside the crate, which use
    /// `transition_epistemic_locked` directly). NOT exposed through any
    /// protocol surface (MCP/REST/shell): agents must use the semantic ops
    /// (`observe`, `assert_knowledge`, `verify_knowledge`, `contradict`,
    /// `supersede`, `merge`, `invalidate`, `resolve_conflict`).
    ///
    /// v0.3 K2 supersession semantics: moving to `Superseded` ends the fact's
    /// validity now (stamps `valid_to` when absent) and, when `superseded_by`
    /// names the successor, records the `SUPERSEDES` edge on the superseded
    /// KO. Supersession lives on the epistemic path — the review's own
    /// doctrine keeps epistemic ("do we still hold this") orthogonal to
    /// lifecycle ("is this record maintained").
    pub fn admin_transition_epistemic(
        &self,
        ctx: impl Into<KnowledgeContext>,
        koid: &KOID,
        to: EpistemicStatus,
        origin: Origin,
        superseded_by: Option<KOID>,
        expected_version: Option<u64>,
        reason: Option<String>,
    ) -> KResult<EpistemicChanged> {
        let ctx = ctx.into();
        let mut pipe = self.pipe.lock().unwrap();
        self.transition_epistemic_locked(
            &mut pipe,
            &ctx,
            koid,
            to,
            origin,
            superseded_by,
            expected_version,
            reason,
        )
    }

    /// admin_transition_epistemic() with the pipe lock already held — internal to
    /// composite knowledge ops (K4).
    pub(crate) fn transition_epistemic_locked(
        &self,
        pipe: &mut Pipeline,
        ctx: &KnowledgeContext,
        koid: &KOID,
        to: EpistemicStatus,
        origin: Origin,
        superseded_by: Option<KOID>,
        expected_version: Option<u64>,
        reason: Option<String>,
    ) -> KResult<EpistemicChanged> {
        let head = self.head_object(koid)?.ok_or(KError::NotFound(*koid))?;
        self.auth
            .read()
            .unwrap()
            .authorize(&ctx.subject, &head, Action::Write)?;
        let from = head.epistemic_status();
        if !from.can_transition(to) {
            return Err(KError::InvalidEpistemic { from, to });
        }
        let cur_v = head.version;
        let expected = expected_version.unwrap_or(cur_v);
        if expected != cur_v {
            return Err(KError::VersionConflict {
                koid: *koid,
                expected,
                found: cur_v,
            });
        }
        let at = self.clock.millis();
        let mut ko = head.clone();
        ko.version = cur_v + 1;
        ko.set_epistemic_status(to);
        if to == EpistemicStatus::Superseded {
            ko.close_valid_time(at)?;
            if let Some(target) = superseded_by {
                if self.head_object(&target)?.is_none() {
                    return Err(KError::InvalidObject(format!(
                        "superseded_by target not found: {}",
                        target.to_hex()
                    )));
                }
                ko.relationships.push(RelationshipRef {
                    rel_type: SUPERSEDES.into(),
                    target,
                    direction: Direction::Outbound,
                });
            }
        } else if superseded_by.is_some() {
            return Err(KError::InvalidObject(
                "'superseded_by' requires a transition to 'superseded'".into(),
            ));
        }
        ko.push_epistemic_history(from, to, at, &ctx.subject.name, reason.as_deref());
        let (commit_ts, _seq) = self.commit_version(
            pipe,
            ko,
            EventKind::EpistemicChanged,
            origin,
            &ctx.subject.name,
            reason,
            None,
            Some(&head.relationships),
        )?;
        Ok(EpistemicChanged {
            koid: *koid,
            version: cur_v + 1,
            commit_ts,
            from,
            to,
        })
    }

    // ---- forget (MRFC-0011 §6.2) -------------------------------------------

    pub fn forget(
        &self,
        ctx: impl Into<KnowledgeContext>,
        koid: &KOID,
        mode: ForgetMode,
        expected_version: Option<u64>,
        note: Option<String>,
    ) -> KResult<Forgotten> {
        let ctx = ctx.into();
        let mut pipe = self.pipe.lock().unwrap();
        let head = self.head_object(koid)?.ok_or(KError::NotFound(*koid))?;
        self.auth
            .read()
            .unwrap()
            .authorize(&ctx.subject, &head, Action::Delete)?;
        let cur_v = head.version;
        let expected = expected_version.unwrap_or(cur_v);
        if expected != cur_v {
            return Err(KError::VersionConflict {
                koid: *koid,
                expected,
                found: cur_v,
            });
        }
        match mode {
            ForgetMode::Tombstone => {
                let mut ko = head.clone();
                ko.version = cur_v + 1;
                ko.lifecycle = Lifecycle {
                    state: LifecycleState::Deleted,
                    origin: Origin::System,
                };
                let (commit_ts, _) = self.commit_version(
                    &mut pipe,
                    ko,
                    EventKind::Forgotten,
                    Origin::System,
                    &ctx.subject.name,
                    note,
                    None,
                    Some(&head.relationships),
                )?;
                Ok(Forgotten {
                    koid: *koid,
                    version: cur_v + 1,
                    commit_ts,
                })
            }
            ForgetMode::Erase => {
                // Legal erasure: remove all versions + head; keep journal and a
                // hash-only stub so `prove` can still verify the chain (GDPR-class).
                let head_payload = codec::encode_ko(&head);
                let head_hash = sha256(&head_payload);
                let signature = self.signing_key.map(|key| hmac_sha256(&key, &head_payload));
                let versions: Vec<u64> = self
                    .repo
                    .scan_object_versions(koid)?
                    .into_iter()
                    .map(|(ts, _)| ts)
                    .collect();
                let commit_ts = self.hlc.now(self.clock.as_ref());
                let seq = pipe.seq + 1;
                let audit = audit_hash_of(
                    pipe.audit,
                    seq,
                    koid,
                    cur_v,
                    EventKind::Forgotten,
                    commit_ts,
                    &head_hash,
                    signature.as_ref(),
                    &ctx.subject.name,
                    note.as_deref(),
                );
                let ke = KnowledgeEvent {
                    seq,
                    koid: *koid,
                    version: cur_v,
                    kind: EventKind::Forgotten,
                    origin: Origin::System,
                    actor: ctx.subject.name.clone(),
                    commit_ts,
                    payload_hash: head_hash,
                    prev_audit_hash: pipe.audit,
                    audit_hash: audit,
                    signature,
                    note,
                };
                let mut batch = WriteBatch::new();
                self.repo.put_event(&mut batch, seq, &ke);
                self.repo.put_journal(&mut batch, seq, audit, commit_ts);
                self.repo.put_tombstone(&mut batch, koid, head_hash, seq);
                self.repo.delete_head(&mut batch, koid);
                // R9: the head is gone — drop the type-index entry with it.
                self.repo
                    .delete_type_index(&mut batch, &head.metadata.type_name, koid);
                for ts in versions {
                    self.repo.delete_object_version(&mut batch, koid, ts);
                }
                self.repo.write_batch(&batch)?;
                pipe.seq = seq;
                pipe.audit = audit;
                self.broadcast(&ke);
                Ok(Forgotten {
                    koid: *koid,
                    version: cur_v,
                    commit_ts,
                })
            }
        }
    }

    // ---- reads (snapshot-isolated, MRFC-0001 §8) ----------------------------

    /// Look up a KO by idempotency key. Returns (koid, version, commit_ts).
    ///
    /// Re-ingest uses this to convert exact-once creates into true updates:
    /// `remember` with an existing idempotency key replays the old write
    /// without storing anything, so an updater must resolve the key first and
    /// remember with an explicit `koid` instead.
    pub fn resolve_idempotency(&self, key: &str) -> KResult<Option<(KOID, u64, u64)>> {
        self.repo.get_idem(key)
    }

    pub fn get(&self, ctx: impl Into<KnowledgeContext>, koid: &KOID) -> KResult<KnowledgeObject> {
        let ctx = ctx.into();
        let mut ko = self.head_object(koid)?.ok_or(KError::NotFound(*koid))?;
        self.auth
            .read()
            .unwrap()
            .authorize(&ctx.subject, &ko, Action::Read)?;
        // Field-level decryption (MRFC-0020 Phase 3).
        if let Some(ref fc) = self.field_crypto {
            if let Some(policy) = self
                .encryption_policies
                .read()
                .unwrap()
                .get(&ko.metadata.type_name)
            {
                let tenant = ctx.tenant.as_deref().unwrap_or("default");
                fc.decrypt_fields(tenant, &ko.metadata.type_name, &mut ko.properties, policy)
                    .map_err(|e| KError::Store(format!("field decrypt: {}", e)))?;
            }
        }
        Ok(ko)
    }

    /// SE2-M25 — batch KO lookups under one auth guard; the same rules as
    /// per-target `get` (a missing KO fails the whole batch with NotFound,
    /// per-object field decryption).
    pub fn get_many(
        &self,
        ctx: impl Into<KnowledgeContext>,
        koids: &[KOID],
    ) -> KResult<Vec<KnowledgeObject>> {
        let ctx = ctx.into();
        let mut kos = self.objects.get_many(koids)?;
        {
            let auth = self.auth.read().unwrap();
            for ko in &kos {
                auth.authorize(&ctx.subject, ko, Action::Read)?;
            }
        }
        // Field-level decryption (MRFC-0020 Phase 3), one policy lock for
        // the batch.
        if let Some(ref fc) = self.field_crypto {
            let policies = self.encryption_policies.read().unwrap();
            let tenant = ctx.tenant.as_deref().unwrap_or("default");
            for ko in &mut kos {
                if let Some(policy) = policies.get(&ko.metadata.type_name) {
                    fc.decrypt_fields(tenant, &ko.metadata.type_name, &mut ko.properties, policy)
                        .map_err(|e| KError::Store(format!("field decrypt: {}", e)))?;
                }
            }
        }
        Ok(kos)
    }

    pub fn get_at(
        &self,
        ctx: impl Into<KnowledgeContext>,
        koid: &KOID,
        snap_ts: u64,
    ) -> KResult<KnowledgeObject> {
        let ctx = ctx.into();
        let ko = self
            .object_at(koid, snap_ts)?
            .ok_or(KError::NotFound(*koid))?;
        self.auth
            .read()
            .unwrap()
            .authorize(&ctx.subject, &ko, Action::Read)?;
        Ok(ko)
    }

    // ---- v0.3 K2: temporal reads -------------------------------------------

    /// Current wall-clock millis from the kernel clock — "now" for valid-time
    /// evaluation. Distinct from HLC commit timestamps.
    pub fn clock_now(&self) -> u64 {
        self.clock.millis()
    }

    /// Point-in-time (transaction-time) read: the version this kernel had
    /// committed as of wall-clock `at_millis`. Packs to the HLC layout
    /// (`millis << 16 | counter`) so the MVCC `<= snap` comparison selects
    /// the newest version committed at or before that instant; `Ok(None)`
    /// when the KO did not exist (or was not yet committed) by then.
    pub fn get_as_of(
        &self,
        ctx: impl Into<KnowledgeContext>,
        koid: &KOID,
        at_millis: u64,
    ) -> KResult<Option<KnowledgeObject>> {
        let ctx = ctx.into();
        let snap = at_millis.checked_shl(16).unwrap_or(u64::MAX);
        let Some(ko) = self.object_at(koid, snap)? else {
            return Ok(None);
        };
        self.auth
            .read()
            .unwrap()
            .authorize(&ctx.subject, &ko, Action::Read)?;
        Ok(Some(ko))
    }

    /// All committed versions of `koid` in ascending commit order —
    /// historical reconstruction for the `HISTORICAL` query operator.
    /// Tombstone (Deleted) versions are skipped; each version is
    /// ACL-checked independently (a version's ACL may differ from the head).
    pub fn history(
        &self,
        ctx: impl Into<KnowledgeContext>,
        koid: &KOID,
    ) -> KResult<Vec<(u64, KnowledgeObject)>> {
        let ctx = ctx.into();
        let mut out = Vec::new();
        for (ts, ko) in self.objects.scan_versions(koid)? {
            if ko.lifecycle.state == LifecycleState::Deleted {
                continue;
            }
            if self
                .auth
                .read()
                .unwrap()
                .authorize(&ctx.subject, &ko, Action::Read)
                .is_err()
            {
                continue;
            }
            out.push((ts, ko));
        }
        Ok(out)
    }

    // ---- v0.3 K3: derivation (anti-CRUD-cosplay, review H4/H6) ---------------

    /// Derive a new KO from premise KOs. This is a real operation, not a
    /// property write: every premise must exist and be readable; the derived
    /// KO carries a first-class Derivation record (WHY / FROM WHAT / HOW /
    /// BY WHOM / WHEN), inbound DERIVED_FROM edges to each source (so
    /// `outbound_edges(src, "derived_from")` finds dependents), the canonical
    /// evidence trail when supplied, and a confidence context (explicit or
    /// baseline-derived from the sources — never silently full). Origin is
    /// Reason, so the epistemic baseline is Inferred.
    pub fn derive(&self, req: DeriveRequest) -> KResult<Remembered> {
        let mut rels = Vec::with_capacity(req.sources.len());
        let mut src_conf: Vec<f32> = Vec::new();
        // Review P1-8 (Model B): a derivation with no explicit evidence
        // inherits the sources' evidence trails — the derived claim is backed
        // by the premises that produced it, and the Derivation record keeps
        // who/how/why. Strict decode: a corrupt source trail is an error,
        // not something to inherit silently (P2-6).
        let mut inherited_evidence: Vec<crate::knowledge::evidence::Evidence> = Vec::new();
        for s in &req.sources {
            let src = self.head_object(s)?.ok_or(KError::NotFound(*s))?;
            self.auth
                .read()
                .unwrap()
                .authorize(&req.context.subject, &src, Action::Read)?;
            rels.push(RelationshipRef {
                rel_type: DERIVED_FROM.into(),
                target: *s,
                direction: Direction::Inbound,
            });
            if let Some(c) = src.confidence_context() {
                src_conf.push(c.score);
            }
            if req.evidence.is_empty() {
                inherited_evidence.extend(src.strict_evidence()?);
            }
        }
        let at = self.clock_now();
        let derivation = Derivation {
            operation: req.operation,
            actor: req.actor,
            model: req.model,
            timestamp: at,
            sources: req.sources.clone(),
            reason: req.reason.clone(),
        };
        // Review P1-7: a caller-supplied confidence override crosses the
        // model boundary here — validated, never trusted.
        let confidence = match req.confidence {
            Some(c) => ConfidenceContext {
                verification_keys: c.verification_keys,
                ..ConfidenceContext::new(c.score, c.confirmations, c.last_verified)?
            },
            None => {
                if src_conf.is_empty() {
                    ConfidenceContext::new(0.0, 0, None).expect("0.0 is in range")
                } else {
                    ConfidenceContext::new(
                        src_conf.iter().sum::<f32>() / src_conf.len() as f32,
                        src_conf.len() as u32,
                        None,
                    )
                    .expect("mean of in-range scores is in range")
                }
            }
        };
        let mut remember = RememberRequest::create(
            req.context,
            Metadata {
                type_name: req.type_name,
                tenant: None,
                schema_version: 1,
                tags: vec![],
            },
        );
        remember.properties = req.properties;
        remember.relationships = rels;
        remember.origin = Origin::Reason;
        remember.note = req.reason.clone();
        remember.extensions.insert(
            KnowledgeObject::EXT_DERIVATION.into(),
            derivation_to_value(&derivation),
        );
        remember.extensions.insert(
            KnowledgeObject::EXT_CONFIDENCE.into(),
            confidence_to_value(&confidence),
        );
        if !req.evidence.is_empty() {
            remember.extensions.insert(
                KnowledgeObject::EXT_EVIDENCE.into(),
                KnowledgeObject::evidence_value(&req.evidence),
            );
        } else if !inherited_evidence.is_empty() {
            remember.extensions.insert(
                KnowledgeObject::EXT_EVIDENCE.into(),
                KnowledgeObject::evidence_value(&inherited_evidence),
            );
        }
        self.remember_trusted(remember)
    }

    // ---- find_similar (MRFC-0011 §6.4) --------------------------------------

    pub fn find_similar(&self, q: SimilarityQuery) -> KResult<Vec<ScoredKO>> {
        let results = self
            .indexes
            .read()
            .unwrap()
            .as_ref()
            // justified: Kernel::open seeds Some(IndexCoordinator) (see open());
            // attach_indexes only swaps Some→Some
            .expect("kernel always has a coordinator")
            .search(self, q)?;
        // v0.3 K2 sibling of the QL Scan filter: similarity recall (vector +
        // BM25 text) also answers with current truth — expired facts stay out
        // of default-time results. Temporal (AS_OF) plans are scan-based and
        // handle time themselves.
        let now = self.clock_now();
        Ok(results.into_iter().filter(|s| s.ko.valid_at(now)).collect())
    }

    /// Type-scoped text search via the IndexCoordinator (BM25 when maintainer
    /// is attached, Jaccard fallback otherwise). Returns `(koid, score,
    /// type_name, version)` tuples like the runtime's `RowSet::Scored`.
    /// Uses `subject` for ACL; callers should pass the same subject used in
    /// the preceding Scan operator.
    pub fn type_scoped_text_search(
        &self,
        subject: &Subject,
        query: &str,
        k: usize,
    ) -> KResult<Vec<(KOID, f32, String, u64)>> {
        let q = SimilarityQuery {
            context: KnowledgeContext::new(subject.clone()),
            filter: None,
            text: Some(query.to_string()),
            vector: None,
            embedding_model: None,
            k,
            fusion: Fusion::TextOnly,
        };
        self.find_similar(q).map(|results| {
            results
                .into_iter()
                .map(|s| {
                    (
                        s.ko.koid,
                        s.score,
                        s.ko.metadata.type_name.clone(),
                        s.ko.version,
                    )
                })
                .collect()
        })
    }

    // ---- type scanning ---------------------------------------------------

    /// Return all readable KOs of a given type (ACL-filtered).
    /// R9: walks the `type/` secondary index (O(log N + per-type)) instead of
    /// the whole head space; the payload type re-check guards against stale
    /// index entries from type changes.
    pub fn scan_by_type(
        &self,
        subject: &Subject,
        type_name: &str,
    ) -> KResult<Vec<KnowledgeObject>> {
        self.scan_by_type_filtered(subject, type_name, None)
    }

    /// Scan by type with an optional epistemic-status filter (v0.3 K1).
    /// Legacy KOs answer via the fallback mapping, same as `epistemic_status()`.
    pub fn scan_by_type_filtered(
        &self,
        subject: &Subject,
        type_name: &str,
        status: Option<EpistemicStatus>,
    ) -> KResult<Vec<KnowledgeObject>> {
        let mut out = Vec::new();
        for koid in self.repo.scan_type(type_name)? {
            let Some(ko) = self.head_object(&koid)? else {
                continue;
            };
            if ko.metadata.type_name != type_name {
                continue; // stale index entry (type changed after indexing)
            }
            if ko.lifecycle.state == LifecycleState::Deleted {
                continue;
            }
            if let Some(want) = status {
                if ko.epistemic_status() != want {
                    continue;
                }
            }
            if self
                .auth
                .read()
                .unwrap()
                .authorize(subject, &ko, Action::Read)
                .is_err()
            {
                continue;
            }
            out.push(ko);
        }
        Ok(out)
    }

    /// Return all distinct type names from head objects. O(n) scan;
    /// ponytail: add a type-name index if enumeration becomes frequent.
    pub fn list_types(&self) -> KResult<Vec<String>> {
        let mut types = std::collections::BTreeSet::new();
        for (koid, _version, _ts, state) in self.repo.scan_heads()? {
            if state == LifecycleState::Deleted {
                continue;
            }
            if let Some(ko) = self.head_object(&koid)? {
                types.insert(ko.metadata.type_name);
            }
        }
        Ok(types.into_iter().collect())
    }

    /// Scan all objects of `type_name` and return inferred constraint candidates
    /// (MRFC-0060 Phase C8). Installs no constraints — caller reviews and manually
    /// registers constraints via `register_schema()`.
    pub fn infer_constraints(
        &self,
        subject: &Subject,
        type_name: &str,
    ) -> KResult<Vec<InferenceCandidate>> {
        let schemas = self.schemas.read().unwrap();
        let schema = match schemas.get(type_name) {
            Some(s) => s.clone(),
            None => return Ok(Vec::new()),
        };
        drop(schemas); // release lock before scan
        let kos = self.scan_by_type(subject, type_name)?;
        let engine = InferenceEngine::new();
        Ok(engine.infer(&schema, &kos))
    }

    // ---- relationship index queries ---------------------------------------

    /// Return outbound edges from `koid` using the relationship index.
    /// Each result is `(rel_type, target_koid)`. When `rel_type_filter` is
    /// `Some`, only edges of that type are returned.
    ///
    /// This is a fast index-only scan — it does NOT load the source KO.
    /// Callers must still verify read access on returned targets separately.
    pub fn outbound_edges(
        &self,
        koid: &KOID,
        rel_type_filter: Option<&str>,
    ) -> KResult<Vec<(String, KOID)>> {
        self.relationships.outbound(koid, rel_type_filter)
    }

    /// Return inbound edges to `koid` using the relationship index.
    /// Each result is `(rel_type, source_koid)`.
    pub fn inbound_edges(
        &self,
        koid: &KOID,
        rel_type_filter: Option<&str>,
    ) -> KResult<Vec<(String, KOID)>> {
        self.relationships.inbound(koid, rel_type_filter)
    }

    // ---- trace / explain / prove (MRFC-0011 §6.5–6.7) ------------------------

    pub fn trace(&self, ctx: impl Into<KnowledgeContext>, koid: &KOID) -> KResult<Lineage> {
        let ctx = ctx.into();
        let head = self.head_object(koid)?.ok_or(KError::NotFound(*koid))?;
        self.auth
            .read()
            .unwrap()
            .authorize(&ctx.subject, &head, Action::Read)?;
        let mut versions = Vec::new();
        for (_ts, ko) in self.repo.scan_object_versions(koid)? {
            versions.push(VersionRecord {
                version: ko.version,
                commit_ts: ko.commit_ts,
                origin: ko.lifecycle.origin.clone(),
                state: ko.lifecycle.state,
            });
        }
        let mut events = Vec::new();
        for ke in self.repo.scan_events()? {
            if ke.koid == *koid {
                events.push(ke);
            }
        }
        Ok(Lineage {
            koid: *koid,
            versions,
            events,
        })
    }

    pub fn explain(
        &self,
        ctx: impl Into<KnowledgeContext>,
        koid: &KOID,
        version: Option<u64>,
    ) -> KResult<Explanation> {
        let ctx = ctx.into();
        let ko = match version {
            None => self.head_object(koid)?.ok_or(KError::NotFound(*koid))?,
            Some(v) => {
                let mut found = None;
                for (_ts, ko) in self.repo.scan_object_versions(koid)? {
                    if ko.version == v {
                        found = Some(ko);
                        break;
                    }
                }
                found.ok_or(KError::NotFound(*koid))?
            }
        };
        self.auth
            .read()
            .unwrap()
            .authorize(&ctx.subject, &ko, Action::Read)?;
        let (source, confidence) = match &ko.semantic {
            Some(s) => (s.source.clone(), s.confidence),
            // Semantic ops (assert/observe/supersede/verify) stamp evidence
            // into the kernel-managed EXT_EVIDENCE extension, not `semantic`
            // (P0-1). Surface its first record so provenance still answers
            // "why is this known?" for asserted claims.
            None => match ko.evidence().first() {
                Some(e) => (Some(e.source_artifact.clone()), Some(e.confidence)),
                None => (None, None),
            },
        };
        Ok(Explanation {
            koid: *koid,
            version: ko.version,
            origin: ko.lifecycle.origin.clone(),
            source,
            confidence,
            verified: ko.lifecycle.state == LifecycleState::Verified,
            evidence: ko
                .relationships
                .iter()
                .map(|r| (r.rel_type.clone(), r.target))
                .collect(),
            event_refs: ko.event_refs.clone(),
        })
    }

    pub fn prove(&self, ctx: impl Into<KnowledgeContext>, claim: &KOID) -> KResult<Proof> {
        let ctx = ctx.into();
        let head = match self.head_object(claim) {
            Ok(Some(h)) => h,
            Ok(None) => return Err(KError::NotFound(*claim)),
            Err(KError::Codec(_)) => {
                // undecodable payload is itself detectable tamper evidence
                return Ok(Proof {
                    claim: *claim,
                    events: 0,
                    chain_valid: false,
                    head_audit_hash: [0u8; 32],
                    signatures_verified: false,
                });
            }
            Err(e) => return Err(e),
        };
        self.auth
            .read()
            .unwrap()
            .authorize(&ctx.subject, &head, Action::Read)?;
        let mut prev = [0u8; 32];
        let mut valid = true;
        let mut count = 0u64;
        let mut signatures_verified = true;
        let mut signed_count = 0u64;
        for ke in self.repo.scan_events()? {
            let expect = audit_hash_of(
                prev,
                ke.seq,
                &ke.koid,
                ke.version,
                ke.kind,
                ke.commit_ts,
                &ke.payload_hash,
                ke.signature.as_ref(),
                &ke.actor,
                ke.note.as_deref(),
            );
            if expect != ke.audit_hash || ke.prev_audit_hash != prev {
                valid = false;
                break;
            }
            // payload integrity: object bytes still hash to the committed value.
            // After legal erasure (ForgetMode::Erase) per-version payloads are
            // gone BY DESIGN; a tombstone stub proves erasure was committed and
            // the audit-chain links above still protect the journal itself.
            match self.repo.get_object_version(&ke.koid, ke.commit_ts)? {
                Some(ko) => {
                    let bytes = codec::encode_ko(&ko);
                    if sha256(&bytes) != ke.payload_hash {
                        valid = false;
                        break;
                    }
                    if let Some(sig) = &ke.signature {
                        signed_count += 1;
                        if let Some(key) = self.signing_key {
                            if hmac_sha256(&key, &bytes) != *sig {
                                signatures_verified = false;
                            }
                        }
                    }
                }
                None => {
                    if self.repo.get_tombstone(&ke.koid)?.is_none() {
                        valid = false;
                        break;
                    }
                }
            }
            prev = ke.audit_hash;
            count += 1;
        }
        if valid {
            if let Some((_, audit, _)) = self.repo.journal_head()? {
                if audit != prev {
                    valid = false;
                }
            }
        }
        Ok(Proof {
            claim: *claim,
            events: count,
            chain_valid: valid,
            head_audit_hash: prev,
            signatures_verified: self.signing_key.is_none()
                || (signatures_verified && signed_count > 0),
        })
    }

    // ---- durable CDC subscriptions (MRFC-0015 pre-work) ----------------------

    pub fn subscribe(
        &self,
        id: String,
        filter: EventFilter,
    ) -> KResult<mpsc::Receiver<KnowledgeEvent>> {
        self.events
            .lock()
            .unwrap()
            .subscribe(&self.repo, id, filter)
    }

    pub fn unsubscribe(&self, id: &str) -> KResult<()> {
        self.events.lock().unwrap().unsubscribe(&self.repo, id)
    }

    pub fn ack(&self, id: &str, seq: u64) -> KResult<()> {
        self.events.lock().unwrap().ack(&self.repo, id, seq)
    }

    pub fn replay(&self, id: &str) -> KResult<Vec<KnowledgeEvent>> {
        self.events.lock().unwrap().replay(&self.repo, id)
    }

    /// In-process notification channel (legacy; prefer `subscribe` for durability).
    ///
    /// R4: returns KResult — `subscribe` persists a durable subscription record
    /// via the repo, so a storage failure here must propagate, not panic.
    pub fn notify(&self, filter: EventFilter) -> KResult<mpsc::Receiver<KnowledgeEvent>> {
        let id = format!("__anon__{}", self.new_koid().to_hex());
        self.subscribe(id, filter)
    }

    /// Full journal scan (conformance + debugging).
    pub fn journal(&self) -> KResult<Vec<KnowledgeEvent>> {
        self.repo.scan_events()
    }

    pub fn journal_head(&self) -> KResult<(u64, [u8; 32])> {
        match self.repo.journal_head()? {
            Some((seq, audit, _)) => Ok((seq, audit)),
            None => Ok((0, [0u8; 32])),
        }
    }

    /// REC-002: write a durable snapshot of the store into a fresh database
    /// file at `path` (live backup — works while the kernel holds the store).
    pub fn backup_store_to(&self, path: &std::path::Path) -> KResult<()> {
        self.store.snapshot_to(path)
    }

    /// REC-002: replace the store contents with the snapshot at `path`
    /// (point-in-time restore). In-memory derived state (semantic status,
    /// enrichment indexes) stays stale until the next kernel open — restart
    /// after restore.
    pub fn restore_store_from(&self, path: &std::path::Path) -> KResult<()> {
        self.store.restore_from(path)
    }

    /// KSE-10: rebuild the derived indexes (relo/reli/type) from canonical
    /// ko/ heads. Repair op — repairs stale, missing, or corrupt derived
    /// rows in one atomic batch; canonical knowledge is never touched.
    pub fn rebuild_derived_indexes(&self) -> KResult<DerivedIndexRebuild> {
        self.repo.rebuild_derived_indexes()
    }

    // ---- Programs-as-KOs (MRFC-0030 Phase 7a) ----------------------------

    /// Deploy a Program KO. The program is aikoql stored as a Knowledge Object
    /// of type `aikoql:program`. Like any KO, it gets versioning, provenance,
    /// access control, and audit trail.
    pub fn deploy_program(
        &self,
        name: &str,
        body: &str,
        language: &str,
        subject: &Subject,
    ) -> KResult<Remembered> {
        let mut props = PropertyMap::new();
        props.insert("name".into(), Value::Text(name.to_string()));
        props.insert("body".into(), Value::Text(body.to_string()));
        props.insert("language".into(), Value::Text(language.to_string()));
        props.insert("version".into(), Value::Int(1));
        self.remember_trusted(RememberRequest {
            context: subject.clone().into(),
            koid: None,
            expected_version: Some(0),
            idempotency_key: Some(format!("deploy-program-{}", name)),
            metadata: Metadata {
                type_name: "aikoql:program".into(),
                tenant: None,
                schema_version: 1,
                tags: vec!["program".into(), "active-object".into()],
            },
            properties: props,
            semantic: None,
            relationships: vec![],
            security: Some(SecurityDescriptor {
                owner: subject.name.clone(),
                acl: vec![],
                classification: None,
            }),
            extensions: ExtensionMap::new(),
            origin: Origin::Human,
            note: Some(format!("Deployed program: {}", name)),
            referential_policy: ReferentialPolicy::Permissive,
        })
    }

    /// Update a Program KO to a new version (new body, incremented version counter).
    pub fn update_program(
        &self,
        koid: &KOID,
        new_body: &str,
        subject: &Subject,
    ) -> KResult<Remembered> {
        let ctx = KnowledgeContext::from(subject.clone());
        let ko = self.get(ctx, koid)?;
        if ko.metadata.type_name != "aikoql:program" {
            return Err(KError::InvalidObject("not a program".into()));
        }
        let cur_ver = match ko.properties.get("version") {
            Some(Value::Int(v)) => *v,
            _ => 1,
        };
        let mut props = ko.properties.clone();
        props.insert("body".into(), Value::Text(new_body.to_string()));
        props.insert("version".into(), Value::Int(cur_ver + 1));
        self.remember_trusted(RememberRequest {
            context: subject.clone().into(),
            koid: Some(*koid),
            expected_version: Some(ko.version),
            idempotency_key: Some(format!("update-program-{}", koid.to_hex())),
            metadata: ko.metadata.clone(),
            properties: props,
            semantic: None,
            relationships: ko.relationships.clone(),
            security: Some(ko.security.clone()),
            extensions: ko.extensions.clone(),
            origin: Origin::Human,
            note: Some(format!("Updated program to v{}", cur_ver + 1)),
            referential_policy: ReferentialPolicy::Permissive,
        })
    }

    /// List all deployed programs.
    pub fn list_programs(&self, subject: &Subject) -> KResult<Vec<KnowledgeObject>> {
        self.scan_by_type(subject, "aikoql:program")
    }

    // ---- Policy-as-KO (MRFC-0030 Phase 7b) --------------------------------

    /// Deploy a Policy KO. When evaluated, determines whether an action is allowed.
    pub fn deploy_policy(
        &self,
        name: &str,
        effect: &str,
        principal: &str,
        action: &str,
        resource_type: &str,
        condition: Option<&str>,
        subject: &Subject,
    ) -> KResult<Remembered> {
        let mut props = PropertyMap::new();
        props.insert("name".into(), Value::Text(name.to_string()));
        props.insert("effect".into(), Value::Text(effect.to_string()));
        props.insert("principal".into(), Value::Text(principal.to_string()));
        props.insert("action".into(), Value::Text(action.to_string()));
        props.insert(
            "resource_type".into(),
            Value::Text(resource_type.to_string()),
        );
        if let Some(c) = condition {
            props.insert("condition".into(), Value::Text(c.to_string()));
        }
        self.remember_trusted(RememberRequest {
            context: subject.clone().into(),
            koid: None,
            expected_version: Some(0),
            idempotency_key: Some(format!("deploy-policy-{}", name)),
            metadata: Metadata {
                type_name: "aikoql:policy".into(),
                tenant: None,
                schema_version: 1,
                tags: vec!["policy".into(), "active-object".into()],
            },
            properties: props,
            semantic: None,
            relationships: vec![],
            security: Some(SecurityDescriptor {
                owner: subject.name.clone(),
                acl: vec![],
                classification: None,
            }),
            extensions: ExtensionMap::new(),
            origin: Origin::Human,
            note: Some(format!("Deployed policy: {}", name)),
            referential_policy: ReferentialPolicy::Permissive,
        })
    }

    /// Evaluate all applicable Policy KOs for a (principal, action, resource_type) tuple.
    /// Returns the first Deny or the first Allow found. Policies are evaluated in
    /// version-descending order (newest first).
    pub fn evaluate_policies(
        &self,
        principal: &str,
        action: &Action,
        resource_type: &str,
        subject: &Subject,
    ) -> KResult<Option<String>> {
        let policies = self.scan_by_type(subject, "aikoql:policy")?;
        for p in policies.iter() {
            let pol_principal = match p.properties.get("principal").and_then(|v| match v {
                Value::Text(s) => Some(s.as_str()),
                _ => None,
            }) {
                Some(s) => s,
                None => continue,
            };
            let pol_action = match p.properties.get("action").and_then(|v| match v {
                Value::Text(s) => Some(s.as_str()),
                _ => None,
            }) {
                Some(s) => s,
                None => continue,
            };
            let pol_resource = match p.properties.get("resource_type").and_then(|v| match v {
                Value::Text(s) => Some(s.as_str()),
                _ => None,
            }) {
                Some(s) => s,
                None => continue,
            };
            // Match: principal, action, resource_type must all match.
            if pol_principal != principal && pol_principal != "*" {
                continue;
            }
            let action_str = format!("{:?}", action);
            if pol_action != action_str && pol_action != "*" {
                continue;
            }
            if pol_resource != resource_type && pol_resource != "*" {
                continue;
            }
            let effect = match p.properties.get("effect").and_then(|v| match v {
                Value::Text(s) => Some(s.as_str()),
                _ => None,
            }) {
                Some(s) => s,
                None => continue,
            };
            if effect == "Deny" {
                return Ok(Some(format!("Denied by policy: {}", p.koid.to_hex())));
            }
            if effect == "Allow" {
                return Ok(None);
            } // Allowed, keep checking
        }
        Ok(Some("No matching policy found".into()))
    }

    // ---- Workflow-as-KO (MRFC-0030 Phase 7b) ------------------------------

    /// Deploy a Workflow KO — a DAG of Program KOs.
    pub fn deploy_workflow(
        &self,
        name: &str,
        steps_json: &str,
        subject: &Subject,
    ) -> KResult<Remembered> {
        let mut props = PropertyMap::new();
        props.insert("name".into(), Value::Text(name.to_string()));
        props.insert("steps".into(), Value::Text(steps_json.to_string()));
        self.remember_trusted(RememberRequest {
            context: subject.clone().into(),
            koid: None,
            expected_version: Some(0),
            idempotency_key: Some(format!("deploy-workflow-{}", name)),
            metadata: Metadata {
                type_name: "aikoql:workflow".into(),
                tenant: None,
                schema_version: 1,
                tags: vec!["workflow".into(), "active-object".into()],
            },
            properties: props,
            semantic: None,
            relationships: vec![],
            security: Some(SecurityDescriptor {
                owner: subject.name.clone(),
                acl: vec![],
                classification: None,
            }),
            extensions: ExtensionMap::new(),
            origin: Origin::Human,
            note: Some(format!("Deployed workflow: {}", name)),
            referential_policy: ReferentialPolicy::Permissive,
        })
    }

    // ---- Trigger-as-KO (MRFC-0030 Phase 7b) -------------------------------

    /// Deploy a Trigger KO — fires on matching KnowledgeEvents.
    pub fn deploy_trigger(
        &self,
        name: &str,
        event_kind: &str,
        type_filter: &str,
        program_koid: &str,
        subject: &Subject,
    ) -> KResult<Remembered> {
        let mut props = PropertyMap::new();
        props.insert("name".into(), Value::Text(name.to_string()));
        props.insert("event_kind".into(), Value::Text(event_kind.to_string()));
        props.insert("type_filter".into(), Value::Text(type_filter.to_string()));
        props.insert("program_koid".into(), Value::Text(program_koid.to_string()));
        self.remember_trusted(RememberRequest {
            context: subject.clone().into(),
            koid: None,
            expected_version: Some(0),
            idempotency_key: Some(format!("deploy-trigger-{}", name)),
            metadata: Metadata {
                type_name: "aikoql:trigger".into(),
                tenant: None,
                schema_version: 1,
                tags: vec!["trigger".into(), "active-object".into()],
            },
            properties: props,
            semantic: None,
            relationships: vec![],
            security: Some(SecurityDescriptor {
                owner: subject.name.clone(),
                acl: vec![],
                classification: None,
            }),
            extensions: ExtensionMap::new(),
            origin: Origin::Human,
            note: Some(format!("Deployed trigger: {}", name)),
            referential_policy: ReferentialPolicy::Permissive,
        })
    }

    // ---- Agent KO (MRFC-0030 Phase 7c) ------------------------------------

    /// Deploy an Agent KO — an AI agent definition with prompt, skills, tools, policies.
    pub fn deploy_agent(
        &self,
        name: &str,
        prompt: &str,
        skills_json: &str,
        tools_json: &str,
        policies_json: &str,
        subject: &Subject,
    ) -> KResult<Remembered> {
        let mut props = PropertyMap::new();
        props.insert("name".into(), Value::Text(name.to_string()));
        props.insert("prompt".into(), Value::Text(prompt.to_string()));
        props.insert("skills".into(), Value::Text(skills_json.to_string()));
        props.insert("tools".into(), Value::Text(tools_json.to_string()));
        props.insert("policies".into(), Value::Text(policies_json.to_string()));
        props.insert("version".into(), Value::Int(1));
        self.remember_trusted(RememberRequest {
            context: subject.clone().into(),
            koid: None,
            expected_version: Some(0),
            idempotency_key: Some(format!("deploy-agent-{}", name)),
            metadata: Metadata {
                type_name: "aikoql:agent".into(),
                tenant: None,
                schema_version: 1,
                tags: vec!["agent".into(), "active-object".into()],
            },
            properties: props,
            semantic: None,
            relationships: vec![],
            security: Some(SecurityDescriptor {
                owner: subject.name.clone(),
                acl: vec![],
                classification: None,
            }),
            extensions: ExtensionMap::new(),
            origin: Origin::Human,
            note: Some(format!("Deployed agent: {}", name)),
            referential_policy: ReferentialPolicy::Permissive,
        })
    }

    /// List all deployed agents.
    pub fn list_agents(&self, subject: &Subject) -> KResult<Vec<KnowledgeObject>> {
        self.scan_by_type(subject, "aikoql:agent")
    }

    // ---- Connector KO (MRFC-0030 Phase 7b) --------------------------------

    /// Deploy a Connector KO — external system import/export as a KO.
    pub fn deploy_connector(
        &self,
        name: &str,
        plugin: &str,
        config_json: &str,
        mapping_json: &str,
        subject: &Subject,
    ) -> KResult<Remembered> {
        let mut props = PropertyMap::new();
        props.insert("name".into(), Value::Text(name.to_string()));
        props.insert("plugin".into(), Value::Text(plugin.to_string()));
        props.insert("config".into(), Value::Text(config_json.to_string()));
        props.insert("mapping".into(), Value::Text(mapping_json.to_string()));
        self.remember_trusted(RememberRequest {
            context: subject.clone().into(),
            koid: None,
            expected_version: Some(0),
            idempotency_key: Some(format!("deploy-connector-{}", name)),
            metadata: Metadata {
                type_name: "aikoql:connector".into(),
                tenant: None,
                schema_version: 1,
                tags: vec!["connector".into(), "active-object".into()],
            },
            properties: props,
            semantic: None,
            relationships: vec![],
            security: Some(SecurityDescriptor {
                owner: subject.name.clone(),
                acl: vec![],
                classification: None,
            }),
            extensions: ExtensionMap::new(),
            origin: Origin::Human,
            note: Some(format!("Deployed connector: {}", name)),
            referential_policy: ReferentialPolicy::Permissive,
        })
    }

    /// List all deployed connectors.
    pub fn list_connectors(&self, subject: &Subject) -> KResult<Vec<KnowledgeObject>> {
        self.scan_by_type(subject, "aikoql:connector")
    }

    // ---- View KO (MRFC-0030 Phase 7b) -----------------------------------

    /// Deploy a View KO — a materialized query over the knowledge graph.
    pub fn deploy_view(
        &self,
        name: &str,
        query: &str,
        refresh_seconds: Option<i64>,
        subject: &Subject,
    ) -> KResult<Remembered> {
        let mut props = PropertyMap::new();
        props.insert("name".into(), Value::Text(name.to_string()));
        props.insert("query".into(), Value::Text(query.to_string()));
        if let Some(secs) = refresh_seconds {
            props.insert("refresh_seconds".into(), Value::Int(secs));
        }
        self.remember_trusted(RememberRequest {
            context: subject.clone().into(),
            koid: None,
            expected_version: Some(0),
            idempotency_key: Some(format!("deploy-view-{}", name)),
            metadata: Metadata {
                type_name: "aikoql:view".into(),
                tenant: None,
                schema_version: 1,
                tags: vec!["view".into(), "active-object".into()],
            },
            properties: props,
            semantic: None,
            relationships: vec![],
            security: Some(SecurityDescriptor {
                owner: subject.name.clone(),
                acl: vec![],
                classification: None,
            }),
            extensions: ExtensionMap::new(),
            origin: Origin::Human,
            note: Some(format!("Deployed view: {}", name)),
            referential_policy: ReferentialPolicy::Permissive,
        })
    }

    /// List all deployed views.
    pub fn list_views(&self, subject: &Subject) -> KResult<Vec<KnowledgeObject>> {
        self.scan_by_type(subject, "aikoql:view")
    }

    // ---- Report KO (MRFC-0030 Phase 7b) ---------------------------------

    /// Deploy a Report KO — compliance/analytics report over the knowledge graph.
    pub fn deploy_report(
        &self,
        name: &str,
        template: &str,
        format: &str,
        parameters_json: &str,
        subject: &Subject,
    ) -> KResult<Remembered> {
        let mut props = PropertyMap::new();
        props.insert("name".into(), Value::Text(name.to_string()));
        props.insert("template".into(), Value::Text(template.to_string()));
        props.insert("format".into(), Value::Text(format.to_string()));
        props.insert(
            "parameters".into(),
            Value::Text(parameters_json.to_string()),
        );
        self.remember_trusted(RememberRequest {
            context: subject.clone().into(),
            koid: None,
            expected_version: Some(0),
            idempotency_key: Some(format!("deploy-report-{}", name)),
            metadata: Metadata {
                type_name: "aikoql:report".into(),
                tenant: None,
                schema_version: 1,
                tags: vec!["report".into(), "active-object".into()],
            },
            properties: props,
            semantic: None,
            relationships: vec![],
            security: Some(SecurityDescriptor {
                owner: subject.name.clone(),
                acl: vec![],
                classification: None,
            }),
            extensions: ExtensionMap::new(),
            origin: Origin::Human,
            note: Some(format!("Deployed report: {}", name)),
            referential_policy: ReferentialPolicy::Permissive,
        })
    }

    /// List all deployed reports.
    pub fn list_reports(&self, subject: &Subject) -> KResult<Vec<KnowledgeObject>> {
        self.scan_by_type(subject, "aikoql:report")
    }

    // ---- Benchmark KO (MRFC-0030 Phase 7b) -------------------------------

    /// Deploy a Benchmark KO — versioned, replayable performance test.
    pub fn deploy_benchmark(
        &self,
        name: &str,
        target_query: &str,
        iterations: i64,
        warmup: Option<i64>,
        subject: &Subject,
    ) -> KResult<Remembered> {
        let mut props = PropertyMap::new();
        props.insert("name".into(), Value::Text(name.to_string()));
        props.insert("target_query".into(), Value::Text(target_query.to_string()));
        props.insert("iterations".into(), Value::Int(iterations));
        if let Some(w) = warmup {
            props.insert("warmup".into(), Value::Int(w));
        }
        self.remember_trusted(RememberRequest {
            context: subject.clone().into(),
            koid: None,
            expected_version: Some(0),
            idempotency_key: Some(format!("deploy-benchmark-{}", name)),
            metadata: Metadata {
                type_name: "aikoql:benchmark".into(),
                tenant: None,
                schema_version: 1,
                tags: vec!["benchmark".into(), "active-object".into()],
            },
            properties: props,
            semantic: None,
            relationships: vec![],
            security: Some(SecurityDescriptor {
                owner: subject.name.clone(),
                acl: vec![],
                classification: None,
            }),
            extensions: ExtensionMap::new(),
            origin: Origin::Human,
            note: Some(format!("Deployed benchmark: {}", name)),
            referential_policy: ReferentialPolicy::Permissive,
        })
    }

    /// List all deployed benchmarks.
    pub fn list_benchmarks(&self, subject: &Subject) -> KResult<Vec<KnowledgeObject>> {
        self.scan_by_type(subject, "aikoql:benchmark")
    }

    // ---- Document ingestion (MRFC-0050) ---------------------------------

    /// Deploy a document Knowledge Object from an ingested artifact.
    pub fn deploy_document(
        &self,
        filename: &str,
        mime_type: &str,
        sha256: &str,
        size_bytes: i64,
        page_count: i64,
        char_count: i64,
        status: &str,
        subject: &Subject,
    ) -> KResult<Remembered> {
        let mut props = PropertyMap::new();
        props.insert("filename".into(), Value::Text(filename.to_string()));
        props.insert("mime_type".into(), Value::Text(mime_type.to_string()));
        props.insert("sha256".into(), Value::Text(sha256.to_string()));
        props.insert("size_bytes".into(), Value::Int(size_bytes));
        props.insert("page_count".into(), Value::Int(page_count));
        props.insert("char_count".into(), Value::Int(char_count));
        props.insert("status".into(), Value::Text(status.into()));
        // R8: mark document-ingested content as untrusted
        let mut extensions = ExtensionMap::new();
        extensions.insert(
            KnowledgeObject::EXT_CONTENT_TRUST.into(),
            Value::Text(ContentTrust::Untrusted.as_str().into()),
        );
        self.remember_trusted(RememberRequest {
            context: subject.clone().into(),
            koid: None,
            expected_version: Some(0),
            idempotency_key: Some(format!("deploy-document-{}", sha256)),
            metadata: Metadata {
                type_name: "aikoql:document".into(),
                tenant: None,
                schema_version: 1,
                tags: vec!["document".into(), "ingestion".into()],
            },
            properties: props,
            semantic: None,
            relationships: vec![],
            security: Some(SecurityDescriptor {
                owner: subject.name.clone(),
                acl: vec![],
                classification: None,
            }),
            extensions,
            origin: Origin::Human,
            note: Some(format!("Ingested document: {}", filename)),
            referential_policy: ReferentialPolicy::Permissive,
        })
    }

    /// List all ingested documents.
    pub fn list_documents(&self, subject: &Subject) -> KResult<Vec<KnowledgeObject>> {
        self.scan_by_type(subject, "aikoql:document")
    }

    // ---- ABI version (MRFC-0011 §9) --------------------------------------

    /// Return the ABI version of this kernel. Adapters can check this to
    /// refuse incompatible versions. Bumped on any breaking syscall change.
    pub fn abi_version(&self) -> u32 {
        1
    }

    // ---- Offline-verifiable prove (MRFC-0011 §6.7) ------------------------

    /// Export the full audit chain for a claim so it can be independently
    /// verified without a running kernel. Returns all knowledge events
    /// in the journal plus the current head audit hash.
    pub fn prove_export(&self) -> KResult<OfflineProof> {
        let events = self.repo.scan_events()?;
        let (seq, audit) = self.journal_head()?;
        Ok(OfflineProof {
            abi_version: self.abi_version(),
            journal_seq: seq,
            head_audit_hash: audit,
            events,
        })
    }

    // ---- Class B syscalls (MRFC-0011 §5, §6.10-6.13) ----------------------

    /// Execute a reasoning rule against the knowledge graph.
    /// Returns provenance-tagged claims with `origin=Reason`.
    /// ponytail: synchronous version for Phase 2; full async JobHandle in Phase 3.
    pub fn reason(
        &self,
        rule_type: &str,
        rule_props: PropertyMap,
    ) -> KResult<Vec<KnowledgeObject>> {
        let subject = Subject {
            name: "kernel-reason".into(),
            roles: vec!["admin".into()],
            tenant: None,
        };
        // Scan objects matching the rule's conditions and produce claims.
        let candidates = self.scan_by_type(&subject, rule_type)?;
        let mut claims = Vec::new();
        for ko in candidates {
            let mut match_count = 0usize;
            for (key, expected) in &rule_props {
                if let Some(v) = ko.properties.get(key) {
                    if v == expected {
                        match_count += 1;
                    }
                }
            }
            if match_count == rule_props.len() && !rule_props.is_empty() {
                let mut claim_props = ko.properties.clone();
                claim_props.insert("reasoned_from".into(), Value::Text(ko.koid.to_hex()));
                claims.push(KnowledgeObject {
                    koid: KOID::ZERO,
                    version: 0,
                    commit_ts: 0,
                    metadata: Metadata {
                        type_name: format!("{}-claim", rule_type),
                        tenant: None,
                        schema_version: 1,
                        tags: vec!["reasoned".into()],
                    },
                    properties: claim_props,
                    semantic: None,
                    relationships: vec![],
                    event_refs: vec![],
                    security: SecurityDescriptor {
                        owner: "kernel-reason".into(),
                        acl: vec![],
                        classification: None,
                    },
                    lifecycle: Lifecycle {
                        state: LifecycleState::Draft,
                        origin: Origin::Reason,
                    },
                    extensions: ExtensionMap::new(),
                });
            }
        }
        Ok(claims)
    }

    /// Infer new knowledge from existing objects using similarity matching.
    /// Takes a prototype type and properties, finds similar objects, and
    /// returns them with provenance.
    pub fn infer(
        &self,
        subject: &Subject,
        type_name: &str,
        similarity_text: &str,
    ) -> KResult<Vec<ScoredKO>> {
        self.find_similar(SimilarityQuery {
            context: subject.clone().into(),
            filter: Some(PropertyFilter {
                type_name: Some(type_name.to_string()),
                required: vec![],
            }),
            text: Some(similarity_text.to_string()),
            vector: None,
            embedding_model: None,
            k: 10,
            fusion: Fusion::TextOnly,
        })
    }

    /// Predict properties for a target object based on similar objects.
    /// Returns a merged property map from the top-k most similar objects.
    pub fn predict(
        &self,
        subject: &Subject,
        type_name: &str,
        target_props: &PropertyMap,
        k: usize,
    ) -> KResult<PropertyMap> {
        // Build similarity text from target properties.
        let text: String = target_props
            .values()
            .map(|v| match v {
                Value::Text(s) => s.clone(),
                other => format!("{:?}", other),
            })
            .collect::<Vec<_>>()
            .join(" ");
        let similar = self.infer(subject, type_name, &text)?;
        let mut merged = PropertyMap::new();
        for scored in similar.iter().take(k) {
            for (key, val) in &scored.ko.properties {
                if !merged.contains_key(key) {
                    merged.insert(key.clone(), val.clone());
                }
            }
        }
        merged.insert(
            "predicted_from_count".into(),
            Value::Int(similar.len() as i64),
        );
        Ok(merged)
    }
}

/// An independently-verifiable proof bundle (MRFC-0011 §6.7).
#[derive(Clone, Debug)]
pub struct OfflineProof {
    pub abi_version: u32,
    pub journal_seq: u64,
    pub head_audit_hash: [u8; 32],
    pub events: Vec<KnowledgeEvent>,
}

// ---------------------------------------------------------------------------
// Scoring helpers now live in `crate::index`. Re-export here for the legacy
// unit tests in this module until those tests migrate to the index module.
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) use crate::knowledge::scoring::{cosine, jaccard, tokenize};

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::store::MemoryEngine;
    use crate::storage::store_redb::RedbEngine;
    use std::collections::BTreeMap;

    fn kernel() -> (Kernel, Arc<ManualClock>) {
        let clock = Arc::new(ManualClock::new(1_000));
        let k = Kernel::open(Arc::new(MemoryEngine::new()), clock.clone(), 42).unwrap();
        (k, clock)
    }

    fn meta(t: &str) -> Metadata {
        Metadata {
            type_name: t.into(),
            tenant: None,
            schema_version: 1,
            tags: vec![],
        }
    }

    #[test]
    fn hlc_is_monotonic_under_same_and_regressing_clock() {
        let h = Hlc::new();
        let c = ManualClock::new(100);
        let a = h.now(&c);
        let b = h.now(&c);
        assert!(b > a);
        c.set(1); // regression
        let d = h.now(&c);
        assert!(d > b);
    }

    #[test]
    fn create_then_head_and_snapshot_reads() {
        let (k, clock) = kernel();
        let alice = Subject::new("alice");
        let r = k
            .remember(RememberRequest::create(alice.clone(), meta("fact")))
            .unwrap();
        assert_eq!(r.version, 1);
        let head = k.get(&alice, &r.koid).unwrap();
        assert_eq!(head.version, 1);
        assert_eq!(head.lifecycle.state, LifecycleState::Draft);

        let snap = k.snapshot();
        clock.tick(5);
        let mut req = RememberRequest::update(alice.clone(), r.koid, meta("fact"));
        req.properties.insert("n".into(), Value::Int(2));
        k.remember(req).unwrap();
        let old = k.get_at(&alice, &r.koid, snap).unwrap();
        assert_eq!(old.version, 1);
        let new = k.get(&alice, &r.koid).unwrap();
        assert_eq!(new.version, 2);
    }

    #[test]
    fn durable_subscription_replay_and_ack() {
        let (k, _clock) = kernel();
        let alice = Subject::new("alice");
        let rx = k.subscribe("s1".into(), EventFilter::default()).unwrap();

        let r1 = k
            .remember(RememberRequest::create(alice.clone(), meta("fact")))
            .unwrap();
        let e1 = rx.recv().unwrap();
        assert_eq!(e1.koid, r1.koid);
        assert_eq!(e1.kind, EventKind::Created);

        k.ack("s1", e1.seq).unwrap();
        let replay = k.replay("s1").unwrap();
        assert!(replay.is_empty(), "acked events must not replay");

        let _r2 = k
            .remember(RememberRequest::create(alice.clone(), meta("fact")))
            .unwrap();
        let e2 = rx.recv().unwrap();
        assert!(e2.seq > e1.seq);

        // without acking e2, replay returns it
        let replay = k.replay("s1").unwrap();
        assert_eq!(replay.len(), 1);
        assert_eq!(replay[0].seq, e2.seq);

        k.unsubscribe("s1").unwrap();
        assert!(k.replay("s1").is_err());
    }

    #[test]
    fn durable_subscription_survives_reopen() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("aikoql_sub_reopen_{}.redb", std::process::id()));
        let _ = std::fs::remove_file(&path);

        let clock = Arc::new(ManualClock::new(1_000));
        let engine = Arc::new(RedbEngine::open(path.to_str().unwrap()).unwrap());
        let k = Kernel::open(engine.clone(), clock.clone(), 42).unwrap();
        let alice = Subject::new("alice");

        let _rx = k.subscribe("s1".into(), EventFilter::default()).unwrap();
        let r = k
            .remember(RememberRequest::create(alice.clone(), meta("fact")))
            .unwrap();
        // do not ack — subscription must replay after reopen
        drop(k);

        let k2 = Kernel::open(engine, clock, 42).unwrap();
        let replay = k2.replay("s1").unwrap();
        assert_eq!(replay.len(), 1);
        assert_eq!(replay[0].koid, r.koid);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn cross_agent_policy_allows_via_role_inheritance() {
        let (k, _clock) = kernel();
        let admin = Subject::with_roles("admin", &["admin"]);

        let mut senior = RememberRequest::create(admin.clone(), meta(ROLE_TYPE));
        senior
            .properties
            .insert("name".into(), Value::Text("senior".into()));
        senior
            .properties
            .insert("parents".into(), Value::List(vec![]));
        k.remember(senior).unwrap();

        let mut junior = RememberRequest::create(admin.clone(), meta(ROLE_TYPE));
        junior
            .properties
            .insert("name".into(), Value::Text("junior".into()));
        junior.properties.insert(
            "parents".into(),
            Value::List(vec![Value::Text("senior".into())]),
        );
        k.remember(junior).unwrap();

        let mut policy = RememberRequest::create(admin.clone(), meta(POLICY_TYPE));
        policy
            .properties
            .insert("target_type".into(), Value::Text("shared_note".into()));
        policy.properties.insert(
            "rules".into(),
            Value::List(vec![Value::Map(BTreeMap::from([
                ("principal".into(), Value::Text("senior".into())),
                ("action".into(), Value::Text("read".into())),
                ("effect".into(), Value::Text("allow".into())),
            ]))]),
        );
        k.remember(policy).unwrap();

        let alice = Subject::with_roles("alice", &["junior"]);
        let note = k
            .remember(RememberRequest::create(alice.clone(), meta("shared_note")))
            .unwrap();

        let bob = Subject::with_roles("bob", &["junior"]);
        let got = k.get(&bob, &note.koid).unwrap();
        assert_eq!(got.metadata.type_name, "shared_note");

        let carol = Subject::new("carol");
        assert!(matches!(
            k.get(&carol, &note.koid),
            Err(KError::AccessDenied { .. })
        ));
    }

    #[test]
    fn policy_deny_overrides_allow() {
        let (k, _clock) = kernel();
        let admin = Subject::with_roles("admin", &["admin"]);

        let mut employee = RememberRequest::create(admin.clone(), meta(ROLE_TYPE));
        employee
            .properties
            .insert("name".into(), Value::Text("employee".into()));
        employee
            .properties
            .insert("parents".into(), Value::List(vec![]));
        k.remember(employee).unwrap();

        let mut intern = RememberRequest::create(admin.clone(), meta(ROLE_TYPE));
        intern
            .properties
            .insert("name".into(), Value::Text("intern".into()));
        intern.properties.insert(
            "parents".into(),
            Value::List(vec![Value::Text("employee".into())]),
        );
        k.remember(intern).unwrap();

        let mut policy = RememberRequest::create(admin.clone(), meta(POLICY_TYPE));
        policy
            .properties
            .insert("target_type".into(), Value::Text("shared_note".into()));
        policy.properties.insert(
            "rules".into(),
            Value::List(vec![
                Value::Map(BTreeMap::from([
                    ("principal".into(), Value::Text("employee".into())),
                    ("action".into(), Value::Text("read".into())),
                    ("effect".into(), Value::Text("allow".into())),
                ])),
                Value::Map(BTreeMap::from([
                    ("principal".into(), Value::Text("intern".into())),
                    ("action".into(), Value::Text("read".into())),
                    ("effect".into(), Value::Text("deny".into())),
                ])),
            ]),
        );
        k.remember(policy).unwrap();

        let alice = Subject::with_roles("alice", &["employee"]);
        let note = k
            .remember(RememberRequest::create(alice.clone(), meta("shared_note")))
            .unwrap();
        assert!(k.get(&alice, &note.koid).is_ok());

        let bob = Subject::with_roles("bob", &["intern"]);
        assert!(matches!(
            k.get(&bob, &note.koid),
            Err(KError::AccessDenied { .. })
        ));
    }

    #[test]
    fn cross_agent_acl_with_role_inheritance() {
        let (k, _clock) = kernel();
        let alice = Subject::new("alice");
        let sec = SecurityDescriptor {
            owner: "alice".into(),
            acl: vec![AclEntry {
                principal: "senior".into(),
                action: Action::Read,
                effect: Effect::Allow,
            }],
            classification: None,
        };
        let mut req = RememberRequest::create(alice.clone(), meta("shared_note"));
        req.security = Some(sec);
        let note = k.remember(req).unwrap();

        let bob = Subject::with_roles("bob", &["senior"]);
        assert!(k.get(&bob, &note.koid).is_ok());

        let carol = Subject::with_roles("carol", &["junior"]);
        assert!(matches!(
            k.get(&carol, &note.koid),
            Err(KError::AccessDenied { .. })
        ));

        let admin = Subject::with_roles("admin", &["admin"]);
        let mut junior = RememberRequest::create(admin.clone(), meta(ROLE_TYPE));
        junior
            .properties
            .insert("name".into(), Value::Text("junior".into()));
        junior.properties.insert(
            "parents".into(),
            Value::List(vec![Value::Text("senior".into())]),
        );
        k.remember(junior).unwrap();

        assert!(k.get(&carol, &note.koid).is_ok());
    }

    #[test]
    fn cosine_and_jaccard_behave() {
        assert!((cosine(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
        assert!(cosine(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6);
        assert_eq!(cosine(&[1.0], &[1.0, 2.0]), 0.0); // dim mismatch
        let a = tokenize("the agent remembered everything");
        let b = tokenize("agent remembered");
        let j = jaccard(&a, &b);
        assert!(j > 0.0 && j < 1.0);
    }
}
