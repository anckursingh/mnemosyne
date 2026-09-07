//! Knowledge Object Model (KOM) — canonical types per MRFC-0001.
//!
//! Normative mapping:
//! - MRFC-0001 §4 req 1–3: every persisted entity is a KO with one immutable KOID;
//!   every mutation creates a new logical version (enforced by the commit pipeline).
//! - MRFC-0001 §5: canonical KO blocks (Identity..Extensions).
//! - MRFC-0001 §6: lifecycle state machine, illegal transitions => deterministic error.
//! - MRFC-0001 §11: error model (extended by MRFC-0011 §8).
//!
//! This module is std-only and free of I/O so it stays deterministic and
//! model-checkable (`loom` in later increments).

use std::collections::{BTreeMap, HashSet};
use std::fmt;

use regex::Regex;

// ---------------------------------------------------------------------------
// Referential integrity policy (MRFC-0001 §7)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ReferentialPolicy {
    /// Every RelationshipRef target must resolve to an existing head object.
    Strict,
    /// Relationship targets are not validated; dangling refs are allowed.
    #[default]
    Permissive,
    /// Full ontology enforcement: domain, range, and cardinality checks (MRFC-0060 C3).
    Enforced,
}

impl ReferentialPolicy {
    pub fn tag(self) -> u8 {
        match self {
            ReferentialPolicy::Strict => 0,
            ReferentialPolicy::Permissive => 1,
            ReferentialPolicy::Enforced => 2,
        }
    }
    pub fn from_tag(t: u8) -> Option<Self> {
        match t {
            0 => Some(ReferentialPolicy::Strict),
            1 => Some(ReferentialPolicy::Permissive),
            2 => Some(ReferentialPolicy::Enforced),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Identity (MRFC-0001 §3: KOID = immutable global identifier)
// ---------------------------------------------------------------------------

pub const KOID_LEN: usize = 16;

/// Immutable global Knowledge Object identifier.
/// Layout (big-endian): 48-bit epoch millis | 32-bit per-millis counter | 48-bit generator salt.
/// Time-ordered so KOIDs have good locality in ordered KV stores.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct KOID(pub [u8; KOID_LEN]);

impl KOID {
    pub const ZERO: KOID = KOID([0u8; KOID_LEN]);

    pub fn from_bytes(b: [u8; KOID_LEN]) -> Self {
        KOID(b)
    }

    pub fn as_bytes(&self) -> &[u8; KOID_LEN] {
        &self.0
    }

    pub fn to_hex(&self) -> String {
        let mut s = String::with_capacity(KOID_LEN * 2);
        for b in &self.0 {
            s.push_str(&format!("{:02x}", b));
        }
        s
    }

    /// Parse a 32-char hex string (as produced by `to_hex`) back into a KOID.
    pub fn from_hex(s: &str) -> KResult<Self> {
        let s = s.trim();
        let b = s.as_bytes();
        if b.len() != KOID_LEN * 2 {
            return Err(KError::InvalidObject(format!(
                "koid hex must be {} chars, got {} (value: '{}')",
                KOID_LEN * 2,
                b.len(),
                if s.len() > 50 {
                    format!("{}...", &s[..47])
                } else {
                    s.to_string()
                }
            )));
        }
        let mut out = [0u8; KOID_LEN];
        for i in 0..KOID_LEN {
            let hi = (b[i * 2] as char).to_digit(16);
            let lo = (b[i * 2 + 1] as char).to_digit(16);
            match (hi, lo) {
                (Some(h), Some(l)) => out[i] = ((h << 4) | l) as u8,
                _ => {
                    return Err(KError::InvalidObject(
                        "koid hex contains non-hex char".into(),
                    ))
                }
            }
        }
        Ok(KOID(out))
    }
}

impl fmt::Debug for KOID {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "KOID({})", self.to_hex())
    }
}

impl fmt::Display for KOID {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_hex())
    }
}

/// Monotonic, seedable KOID generator. Deterministic given the same id-space
/// seed and clock sequence — a hard requirement for conformance replay
/// (MRFC-0011 §11). The seed is an ID-space namespace, not cryptographic
/// material (CodeQL FP: the old `salt` name tripped the hardcoded-value sink).
pub struct IdGen {
    id_seed: u64,
    last_ms: u64,
    counter: u32,
}

impl IdGen {
    pub fn new(id_seed: u64) -> Self {
        IdGen {
            id_seed,
            last_ms: 0,
            counter: 0,
        }
    }

    pub fn next(&mut self, now_ms: u64) -> KOID {
        let ms = if now_ms > self.last_ms {
            self.counter = 0;
            now_ms
        } else {
            self.counter = self.counter.wrapping_add(1);
            self.last_ms
        };
        self.last_ms = ms;
        let mut b = [0u8; KOID_LEN];
        b[0..6].copy_from_slice(&ms.to_be_bytes()[2..8]);
        b[6..10].copy_from_slice(&self.counter.to_be_bytes());
        b[10..16].copy_from_slice(&self.id_seed.to_be_bytes()[2..8]);
        KOID(b)
    }
}

// ---------------------------------------------------------------------------
// Properties (MRFC-0001 §5: Properties + Extensions blocks)
// ---------------------------------------------------------------------------

/// Canonical property value. Map keys are sorted (BTreeMap) so encoding is canonical.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
    Bytes(Vec<u8>),
    List(Vec<Value>),
    Map(BTreeMap<String, Value>),
}

pub type PropertyMap = BTreeMap<String, Value>;

// ---------------------------------------------------------------------------
// Value type introspection (MRFC-0060 Phase C1)
// ---------------------------------------------------------------------------

impl Value {
    /// Return the aikoql type name for this value.
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Null => "Null",
            Value::Bool(_) => "Bool",
            Value::Int(_) => "Int",
            Value::Float(_) => "Float",
            Value::Text(_) => "Text",
            Value::Bytes(_) => "Bytes",
            Value::List(_) => "List",
            Value::Map(_) => "Map",
        }
    }

    /// Validate this value against a declared schema property type.
    /// Returns `Ok(())` if the value matches the expected type, or if the
    /// value is Null and the property is nullable.
    pub fn type_check(&self, prop: &SchemaProperty) -> Result<(), String> {
        match self {
            Value::Null => {
                if prop.nullable {
                    Ok(())
                } else {
                    Err(format!(
                        "property '{}' is not nullable but got Null",
                        prop.name
                    ))
                }
            }
            Value::Bool(_) => {
                if prop.value_type == "Bool" {
                    Ok(())
                } else {
                    Err(format!(
                        "property '{}' type mismatch: expected {}, got Bool",
                        prop.name, prop.value_type
                    ))
                }
            }
            Value::Int(_) => {
                if prop.value_type == "Int" {
                    Ok(())
                } else if prop.value_type == "Float" {
                    // Int → Float is a widening conversion, accept it
                    Ok(())
                } else {
                    Err(format!(
                        "property '{}' type mismatch: expected {}, got Int",
                        prop.name, prop.value_type
                    ))
                }
            }
            Value::Float(_) => {
                if prop.value_type == "Float" {
                    Ok(())
                } else {
                    Err(format!(
                        "property '{}' type mismatch: expected {}, got Float",
                        prop.name, prop.value_type
                    ))
                }
            }
            Value::Text(_) => {
                if prop.value_type == "Text"
                    || prop.value_type == "DateTime"
                    || prop.value_type == "Json"
                {
                    Ok(())
                } else {
                    Err(format!(
                        "property '{}' type mismatch: expected {}, got Text",
                        prop.name, prop.value_type
                    ))
                }
            }
            Value::Bytes(_) => {
                if prop.value_type == "Bytes" {
                    Ok(())
                } else {
                    Err(format!(
                        "property '{}' type mismatch: expected {}, got Bytes",
                        prop.name, prop.value_type
                    ))
                }
            }
            Value::List(_) => {
                if prop.value_type == "List" {
                    Ok(())
                } else {
                    Err(format!(
                        "property '{}' type mismatch: expected {}, got List",
                        prop.name, prop.value_type
                    ))
                }
            }
            Value::Map(_) => {
                if prop.value_type == "Map" || prop.value_type == "Json" {
                    Ok(())
                } else {
                    Err(format!(
                        "property '{}' type mismatch: expected {}, got Map",
                        prop.name, prop.value_type
                    ))
                }
            }
        }
    }
}

/// Unknown extension fields MUST survive round-trip serialization (MRFC-0001 req 9).
pub type ExtensionMap = BTreeMap<String, Value>;

#[derive(Clone, Debug, PartialEq)]
pub struct Metadata {
    pub type_name: String,
    pub tenant: Option<String>,
    pub schema_version: u32,
    pub tags: Vec<String>,
}

/// Semantic metadata is OPTIONAL (MRFC-0001 req 8) and never mutated by storage
/// (MRFC-0001 §13). Vectors are namespaced by `embedding_model` (review R7).
#[derive(Clone, Debug, PartialEq)]
pub struct SemanticBlock {
    pub embedding_model: Option<String>,
    pub embedding: Option<Vec<f32>>,
    pub confidence: Option<f32>,
    pub source: Option<String>,
    pub summary: Option<String>,
}

// ---------------------------------------------------------------------------
// Relationships & Events (MRFC-0001 §3: KR / KE)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Outbound,
    Inbound,
}

impl Direction {
    pub fn tag(self) -> u8 {
        match self {
            Direction::Outbound => 0,
            Direction::Inbound => 1,
        }
    }
    pub fn from_tag(t: u8) -> Option<Self> {
        match t {
            0 => Some(Direction::Outbound),
            1 => Some(Direction::Inbound),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct RelationshipRef {
    pub rel_type: String,
    pub target: KOID,
    pub direction: Direction,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventKind {
    Created,
    Updated,
    Forgotten,
    LifecycleChanged,
    ClaimAsserted,
    Audit,
    /// v0.3 K1: epistemic status transition (audit-trailed, like lifecycle).
    EpistemicChanged,
}

impl EventKind {
    pub fn tag(self) -> u8 {
        match self {
            EventKind::Created => 0,
            EventKind::Updated => 1,
            EventKind::Forgotten => 2,
            EventKind::LifecycleChanged => 3,
            EventKind::ClaimAsserted => 4,
            EventKind::Audit => 5,
            EventKind::EpistemicChanged => 6,
        }
    }
    pub fn from_tag(t: u8) -> Option<Self> {
        match t {
            0 => Some(EventKind::Created),
            1 => Some(EventKind::Updated),
            2 => Some(EventKind::Forgotten),
            3 => Some(EventKind::LifecycleChanged),
            4 => Some(EventKind::ClaimAsserted),
            5 => Some(EventKind::Audit),
            6 => Some(EventKind::EpistemicChanged),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct EventRef {
    pub seq: u64,
    pub kind: EventKind,
    pub commit_ts: u64,
}

/// Who produced a version. Claims from Class B syscalls re-enter the store
/// tagged with non-Human origins (MRFC-0011 §6.10–6.13).
#[derive(Clone, Debug, PartialEq)]
pub enum Origin {
    Human,
    Agent(String),
    SemanticEnrichment,
    Reason,
    System,
}

// ---------------------------------------------------------------------------
// Security (MRFC-0001 §12)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Read,
    Write,
    Evolve,
    Delete,
    Admin,
}

impl Action {
    pub fn tag(self) -> u8 {
        match self {
            Action::Read => 0,
            Action::Write => 1,
            Action::Evolve => 2,
            Action::Delete => 3,
            Action::Admin => 4,
        }
    }
    pub fn from_tag(t: u8) -> Option<Self> {
        match t {
            0 => Some(Action::Read),
            1 => Some(Action::Write),
            2 => Some(Action::Evolve),
            3 => Some(Action::Delete),
            4 => Some(Action::Admin),
            _ => None,
        }
    }
    pub fn from_name(s: &str) -> Option<Self> {
        match s {
            "read" => Some(Action::Read),
            "write" => Some(Action::Write),
            "evolve" => Some(Action::Evolve),
            "delete" => Some(Action::Delete),
            "admin" => Some(Action::Admin),
            _ => None,
        }
    }
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Action::Read => "read",
            Action::Write => "write",
            Action::Evolve => "evolve",
            Action::Delete => "delete",
            Action::Admin => "admin",
        };
        write!(f, "{}", s)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect {
    Allow,
    Deny,
}

impl Effect {
    pub fn tag(self) -> u8 {
        match self {
            Effect::Allow => 0,
            Effect::Deny => 1,
        }
    }
    pub fn from_tag(t: u8) -> Option<Self> {
        match t {
            0 => Some(Effect::Allow),
            1 => Some(Effect::Deny),
            _ => None,
        }
    }
    pub fn from_name(s: &str) -> Option<Self> {
        match s {
            "allow" => Some(Effect::Allow),
            "deny" => Some(Effect::Deny),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct AclEntry {
    /// Principal name or role name.
    pub principal: String,
    pub action: Action,
    pub effect: Effect,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SecurityDescriptor {
    pub owner: String,
    pub acl: Vec<AclEntry>,
    pub classification: Option<String>,
}

// ---------------------------------------------------------------------------
// Lifecycle (MRFC-0001 §6): Draft -> Active -> Verified -> Archived -> Deleted
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LifecycleState {
    // Original MRFC-0001 states (tags 0-4, preserved for backward compat)
    Draft,
    Active,
    Verified,
    Archived,
    Deleted,
    // MRFC-0070 extended states (tags 5-11)
    Discovered,
    Extracted,
    Proposed,
    Validated,
    Accepted,
    Updated,
    Superseded,
}

impl LifecycleState {
    pub fn tag(self) -> u8 {
        match self {
            // Original tags preserved
            LifecycleState::Draft => 0,
            LifecycleState::Active => 1,
            LifecycleState::Verified => 2,
            LifecycleState::Archived => 3,
            LifecycleState::Deleted => 4,
            // New MRFC-0070 states
            LifecycleState::Discovered => 5,
            LifecycleState::Extracted => 6,
            LifecycleState::Proposed => 7,
            LifecycleState::Validated => 8,
            LifecycleState::Accepted => 9,
            LifecycleState::Updated => 10,
            LifecycleState::Superseded => 11,
        }
    }
    pub fn from_tag(t: u8) -> Option<Self> {
        match t {
            0 => Some(LifecycleState::Draft),
            1 => Some(LifecycleState::Active),
            2 => Some(LifecycleState::Verified),
            3 => Some(LifecycleState::Archived),
            4 => Some(LifecycleState::Deleted),
            5 => Some(LifecycleState::Discovered),
            6 => Some(LifecycleState::Extracted),
            7 => Some(LifecycleState::Proposed),
            8 => Some(LifecycleState::Validated),
            9 => Some(LifecycleState::Accepted),
            10 => Some(LifecycleState::Updated),
            11 => Some(LifecycleState::Superseded),
            _ => None,
        }
    }

    /// Full MRFC-0070 knowledge lifecycle transitions.
    /// Legacy path (backward compat):
    ///   Draft → Active → Verified → Archived → Deleted
    /// MRFC-0070 path:
    ///   Discovered → Extracted → Proposed → Validated → Accepted → Active
    ///   Active → Updated (new version) → Superseded (old version)
    ///   Superseded → Archived → Deleted
    /// Cross-compat: Draft ≈ Proposed, Verified ≈ Accepted
    pub fn can_transition(self, to: LifecycleState) -> bool {
        use LifecycleState::*;
        matches!(
            (self, to),
            // Legacy path
            (Draft, Active)
                | (Active, Verified)
                | (Verified, Archived)
                | (Archived, Deleted)
            // MRFC-0070 creation path
                | (Discovered, Extracted)
                | (Extracted, Proposed)
                | (Proposed, Validated)
                | (Validated, Accepted)
                | (Accepted, Active)
            // Active phase — evolution
                | (Active, Updated)
                | (Updated, Superseded)
            // Termination
                | (Superseded, Archived)
            // Cross-compat bridges (new states only — legacy path unchanged)
                | (Draft, Proposed)     // Draft → MRFC-0070 path
                | (Draft, Accepted)     // Draft → Accepted (skip legacy Verified)
                | (Proposed, Active)    // Proposed → Active
                | (Accepted, Archived)  // Accepted → Archived
                | (Accepted, Updated)   // Accepted → Updated
                | (Active, Superseded) // Active → Superseded
        )
    }
}

impl fmt::Display for LifecycleState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            LifecycleState::Draft => "draft",
            LifecycleState::Active => "active",
            LifecycleState::Verified => "verified",
            LifecycleState::Archived => "archived",
            LifecycleState::Deleted => "deleted",
            LifecycleState::Discovered => "discovered",
            LifecycleState::Extracted => "extracted",
            LifecycleState::Proposed => "proposed",
            LifecycleState::Validated => "validated",
            LifecycleState::Accepted => "accepted",
            LifecycleState::Updated => "updated",
            LifecycleState::Superseded => "superseded",
        };
        write!(f, "{}", s)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Lifecycle {
    pub state: LifecycleState,
    pub origin: Origin,
}

// ---------------------------------------------------------------------------
// Canonical Knowledge Object (MRFC-0001 §5)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct KnowledgeObject {
    // Identity block
    pub koid: KOID,
    pub version: u64,
    pub commit_ts: u64,
    // Metadata block
    pub metadata: Metadata,
    // Properties block
    pub properties: PropertyMap,
    // Semantic block (optional)
    pub semantic: Option<SemanticBlock>,
    // RelationshipRefs block
    pub relationships: Vec<RelationshipRef>,
    // EventRefs block
    pub event_refs: Vec<EventRef>,
    // Security block
    pub security: SecurityDescriptor,
    // Lifecycle block
    pub lifecycle: Lifecycle,
    // Extensions block (unknown fields preserved)
    pub extensions: ExtensionMap,
}

impl KnowledgeObject {
    pub fn new(koid: KOID, metadata: Metadata, security: SecurityDescriptor) -> Self {
        KnowledgeObject {
            koid,
            version: 0,
            commit_ts: 0,
            metadata,
            properties: PropertyMap::new(),
            semantic: None,
            relationships: Vec::new(),
            event_refs: Vec::new(),
            security,
            lifecycle: Lifecycle {
                state: LifecycleState::Draft,
                origin: Origin::System,
            },
            extensions: ExtensionMap::new(),
        }
    }

    /// MRFC-0001 §10 validation rules (subset enforceable at the type boundary;
    /// duplicate property identifiers are impossible by construction — BTreeMap).
    pub fn validate(&self) -> KResult<()> {
        if self.metadata.type_name.trim().is_empty() {
            return Err(KError::InvalidObject(
                "metadata.type_name must be non-empty".into(),
            ));
        }
        if self.security.owner.trim().is_empty() {
            return Err(KError::InvalidObject(
                "security.owner must be non-empty".into(),
            ));
        }
        for t in &self.metadata.tags {
            if t.trim().is_empty() {
                return Err(KError::InvalidObject(
                    "metadata.tags must not contain empty entries".into(),
                ));
            }
        }
        for r in &self.relationships {
            if r.rel_type.trim().is_empty() {
                return Err(KError::InvalidObject(
                    "relationships[].rel_type must be non-empty".into(),
                ));
            }
        }
        for entry in &self.security.acl {
            if entry.principal.trim().is_empty() {
                return Err(KError::InvalidObject(
                    "security.acl[].principal must be non-empty".into(),
                ));
            }
        }
        Ok(())
    }

    /// Validate this KO against a registered schema. Makes `KError::InvalidSchema`
    /// reachable and enforces type/version/required-property/unknown-core-field
    /// invariants.
    /// When `skip_not_null` is true, the required-properties loop is skipped
    /// (backend enforces NOT NULL). Type-name/version/closed-world checks
    /// still run — they're structural, not constraint-level.
    /// MRFC-0060 Phase C7.
    pub fn validate_against(&self, schema: &Schema, skip_not_null: bool) -> KResult<()> {
        schema.ensure_allowed_includes_required();
        if self.metadata.type_name != schema.type_name {
            return Err(KError::InvalidSchema(format!(
                "type_name mismatch: expected '{}', got '{}'",
                schema.type_name, self.metadata.type_name
            )));
        }
        if self.metadata.schema_version != schema.schema_version {
            return Err(KError::InvalidSchema(format!(
                "schema_version mismatch: expected {}, got {}",
                schema.schema_version, self.metadata.schema_version
            )));
        }
        if !skip_not_null {
            for req in &schema.required_properties {
                if !self.properties.contains_key(req) {
                    return Err(KError::InvalidSchema(format!(
                        "missing required property: '{}'",
                        req
                    )));
                }
            }
        }
        if let Some(allowed) = &schema.allowed_properties {
            for key in self.properties.keys() {
                if !allowed.contains(key) {
                    return Err(KError::InvalidSchema(format!(
                        "unknown core field: '{}'",
                        key
                    )));
                }
            }
        }
        Ok(())
    }

    // ---- MRFC-0070 Phase A0: Authority & Scope helpers (stored in extensions) ----

    /// Extension key for `Authority` value.
    pub const EXT_AUTHORITY: &str = "authority";
    /// Extension key for `Scope` value.
    pub const EXT_SCOPE: &str = "scope";
    /// Extension key for `ContentTrust` value (R8 remediation).
    pub const EXT_CONTENT_TRUST: &str = "content_trust";

    /// Get the Authority level from extensions, if set.
    pub fn authority(&self) -> Option<crate::knowledge::authority::Authority> {
        self.extensions
            .get(Self::EXT_AUTHORITY)
            .and_then(|v| match v {
                Value::Text(s) => crate::knowledge::authority::Authority::from_str(s),
                _ => None,
            })
    }

    /// Set the Authority level in extensions.
    pub fn set_authority(&mut self, a: crate::knowledge::authority::Authority) {
        self.extensions
            .insert(Self::EXT_AUTHORITY.into(), Value::Text(a.as_str().into()));
    }

    /// Get the Scope from extensions, if set.
    pub fn scope(&self) -> Option<crate::knowledge::scope::Scope> {
        self.extensions.get(Self::EXT_SCOPE).and_then(|v| match v {
            Value::Text(s) => crate::knowledge::scope::Scope::from_str(s),
            _ => None,
        })
    }

    /// Set the Scope in extensions.
    pub fn set_scope(&mut self, s: crate::knowledge::scope::Scope) {
        self.extensions
            .insert(Self::EXT_SCOPE.into(), Value::Text(s.as_str().into()));
    }

    /// Get the ContentTrust level from extensions, if set.
    /// Default: `Unknown` (conservative — treat as untrusted until proven otherwise).
    pub fn content_trust(&self) -> ContentTrust {
        self.extensions
            .get(Self::EXT_CONTENT_TRUST)
            .and_then(|v| match v {
                Value::Text(s) => ContentTrust::from_str(s),
                _ => None,
            })
            // justified: absent/unrecognized extension → Unknown
            // (conservative default, documented in getter doc)
            .unwrap_or_default()
    }

    /// Set the ContentTrust level in extensions.
    pub fn set_content_trust(&mut self, ct: ContentTrust) {
        self.extensions.insert(
            Self::EXT_CONTENT_TRUST.into(),
            Value::Text(ct.as_str().into()),
        );
    }

    // ---- v0.3 K1: Epistemic status helpers (stored in extensions) ----

    /// Extension key for `EpistemicStatus` value.
    pub const EXT_EPISTEMIC_STATUS: &str = "epistemic_status";
    /// Extension key for the append-only status transition history.
    pub const EXT_EPISTEMIC_HISTORY: &str = "epistemic_history";

    /// Current epistemic status. The explicit extension wins; legacy KOs
    /// (written before v0.3 K1) fall back to their lifecycle state:
    /// Verified → Verified, Extracted → Extracted, everything else → Observed.
    pub fn epistemic_status(&self) -> EpistemicStatus {
        self.extensions
            .get(Self::EXT_EPISTEMIC_STATUS)
            .and_then(|v| match v {
                Value::Text(s) => EpistemicStatus::from_str(s),
                _ => None,
            })
            .unwrap_or(match self.lifecycle.state {
                LifecycleState::Verified => EpistemicStatus::Verified,
                LifecycleState::Extracted => EpistemicStatus::Extracted,
                _ => EpistemicStatus::Observed,
            })
    }

    /// Set the epistemic status extension. Low-level — no transition
    /// validation; kernel `transition_epistemic` is the enforced path.
    pub fn set_epistemic_status(&mut self, s: EpistemicStatus) {
        self.extensions.insert(
            Self::EXT_EPISTEMIC_STATUS.into(),
            Value::Text(s.as_str().into()),
        );
    }

    /// Append a transition record to the history extension (append-only:
    /// existing entries are never modified or removed).
    pub fn push_epistemic_history(
        &mut self,
        from: EpistemicStatus,
        to: EpistemicStatus,
        at_millis: u64,
        by: &str,
        reason: Option<&str>,
    ) {
        let mut entry = BTreeMap::new();
        entry.insert("from".into(), Value::Text(from.as_str().into()));
        entry.insert("to".into(), Value::Text(to.as_str().into()));
        entry.insert("at".into(), Value::Int(at_millis as i64));
        entry.insert("by".into(), Value::Text(by.into()));
        if let Some(r) = reason {
            entry.insert("reason".into(), Value::Text(r.into()));
        }
        match self.extensions.get_mut(Self::EXT_EPISTEMIC_HISTORY) {
            Some(Value::List(l)) => l.push(Value::Map(entry)),
            _ => {
                self.extensions.insert(
                    Self::EXT_EPISTEMIC_HISTORY.into(),
                    Value::List(vec![Value::Map(entry)]),
                );
            }
        }
    }

    // ---- v0.3 K1: Evidence helpers (canonical extension encoding) ----

    /// Extension key for the canonical evidence list (R12 immutable prefix).
    pub const EXT_EVIDENCE: &str = "evidence";
    /// Extension key for the append-only lifecycle transition history.
    pub const EXT_LIFECYCLE_HISTORY: &str = "lifecycle_history";

    /// Decode the canonical evidence list. A missing/unrecognized extension
    /// or malformed entries yield empty/skipped records — legacy KOs stored
    /// evidence as flat `evidence_*` properties and are not decoded here.
    pub fn evidence(&self) -> Vec<crate::knowledge::evidence::Evidence> {
        use crate::knowledge::evidence::{Evidence, EvidenceMethod};
        match self.extensions.get(Self::EXT_EVIDENCE) {
            Some(Value::List(items)) => items
                .iter()
                .filter_map(|v| match v {
                    Value::Map(m) => {
                        let source_artifact = match m.get("source_artifact") {
                            Some(Value::Text(s)) => s.clone(),
                            _ => return None,
                        };
                        let method = match m.get("method").and_then(|x| match x {
                            Value::Text(s) => EvidenceMethod::from_str(s),
                            _ => None,
                        }) {
                            Some(method) => method,
                            None => return None,
                        };
                        let mut ev = Evidence::new(source_artifact, method);
                        if let Some(Value::Text(l)) = m.get("location") {
                            ev = ev.with_location(l.clone());
                        }
                        if let Some(Value::Text(r)) = m.get("revision") {
                            ev = ev.with_revision(r.clone());
                        }
                        if let Some(Value::Float(c)) = m.get("confidence") {
                            ev = ev.with_confidence(*c as f32);
                        }
                        Some(ev)
                    }
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        }
    }

    /// Strict variant of `evidence()` for epistemic-critical reads (review
    /// P2-6): a present-but-malformed evidence entry is an error, not a
    /// silent drop — verify/derive/trace must never build on evidence they
    /// half-understand. Absent evidence decodes to an empty trail.
    pub fn strict_evidence(&self) -> KResult<Vec<crate::knowledge::evidence::Evidence>> {
        use crate::knowledge::evidence::{Evidence, EvidenceMethod};
        match self.extensions.get(Self::EXT_EVIDENCE) {
            None => Ok(Vec::new()),
            Some(Value::List(items)) => {
                let mut out = Vec::with_capacity(items.len());
                for (i, v) in items.iter().enumerate() {
                    let m = match v {
                        Value::Map(m) => m,
                        other => {
                            return Err(KError::Codec(format!(
                                "evidence entry {i} is {other:?}, expected a map"
                            )));
                        }
                    };
                    let source_artifact = match m.get("source_artifact") {
                        Some(Value::Text(s)) => s.clone(),
                        _ => {
                            return Err(KError::Codec(format!(
                                "evidence entry {i} is missing source_artifact"
                            )));
                        }
                    };
                    let method = match m.get("method").and_then(|x| match x {
                        Value::Text(s) => EvidenceMethod::from_str(s),
                        _ => None,
                    }) {
                        Some(method) => method,
                        None => {
                            return Err(KError::Codec(format!(
                                "evidence entry {i} has an unknown method"
                            )));
                        }
                    };
                    let mut ev = Evidence::new(source_artifact, method);
                    if let Some(Value::Text(l)) = m.get("location") {
                        ev = ev.with_location(l.clone());
                    }
                    if let Some(Value::Text(r)) = m.get("revision") {
                        ev = ev.with_revision(r.clone());
                    }
                    if let Some(Value::Float(c)) = m.get("confidence") {
                        ev = ev.with_confidence(*c as f32);
                    }
                    out.push(ev);
                }
                Ok(out)
            }
            // Legacy non-list evidence is not canonical (see evidence()) —
            // strict readers refuse it rather than guess.
            Some(other) => Err(KError::Codec(format!(
                "evidence extension is {other:?}, expected a list"
            ))),
        }
    }

    /// Canonical extension value for an evidence trail (v0.3 K1) — public so
    /// ingestion and other producers construct the exact same encoding.
    pub fn evidence_value(evs: &[crate::knowledge::evidence::Evidence]) -> Value {
        Value::List(evs.iter().map(evidence_to_value).collect())
    }

    /// Replace the evidence trail with its canonical encoding. Deterministic
    /// (BTreeMap ordering) — the R12 append-only check compares raw values.
    pub fn set_evidence(&mut self, evs: Vec<crate::knowledge::evidence::Evidence>) {
        self.extensions
            .insert(Self::EXT_EVIDENCE.into(), Self::evidence_value(&evs));
    }

    /// Append one evidence record; exact duplicates are skipped.
    pub fn add_evidence(&mut self, ev: crate::knowledge::evidence::Evidence) {
        let encoded = evidence_to_value(&ev);
        match self.extensions.get_mut(Self::EXT_EVIDENCE) {
            Some(Value::List(l)) => {
                if !l.contains(&encoded) {
                    l.push(encoded);
                }
            }
            _ => {
                self.extensions
                    .insert(Self::EXT_EVIDENCE.into(), Value::List(vec![encoded]));
            }
        }
    }

    /// Append a lifecycle transition record — transitions create evidence,
    /// mirroring `push_epistemic_history`.
    pub fn push_lifecycle_history(
        &mut self,
        from: LifecycleState,
        to: LifecycleState,
        at_millis: u64,
        by: &str,
        reason: Option<&str>,
    ) {
        let mut entry = BTreeMap::new();
        entry.insert("from".into(), Value::Text(from.to_string()));
        entry.insert("to".into(), Value::Text(to.to_string()));
        entry.insert("at".into(), Value::Int(at_millis as i64));
        entry.insert("by".into(), Value::Text(by.into()));
        if let Some(r) = reason {
            entry.insert("reason".into(), Value::Text(r.into()));
        }
        match self.extensions.get_mut(Self::EXT_LIFECYCLE_HISTORY) {
            Some(Value::List(l)) => l.push(Value::Map(entry)),
            _ => {
                self.extensions.insert(
                    Self::EXT_LIFECYCLE_HISTORY.into(),
                    Value::List(vec![Value::Map(entry)]),
                );
            }
        }
    }

    // ---- v0.3 K2: Valid-time helpers (stored in extensions) ----

    /// Extension key for valid_from (epoch millis; absent = unbounded past).
    pub const EXT_VALID_FROM: &str = "valid_from";
    /// Extension key for valid_to (epoch millis; absent = unbounded future).
    pub const EXT_VALID_TO: &str = "valid_to";

    /// Start of the validity interval, epoch millis. None = unbounded past.
    /// Distinct from commit_ts (transaction time) and from `observed_at` —
    /// this is when the knowledge is true in the world (K2 adversarial test:
    /// timeless sentences must not create timeless truth).
    pub fn valid_from(&self) -> Option<u64> {
        match self.extensions.get(Self::EXT_VALID_FROM) {
            Some(Value::Int(v)) if *v >= 0 => Some(*v as u64),
            _ => None,
        }
    }

    /// End of the validity interval (exclusive), epoch millis. None = open.
    pub fn valid_to(&self) -> Option<u64> {
        match self.extensions.get(Self::EXT_VALID_TO) {
            Some(Value::Int(v)) if *v >= 0 => Some(*v as u64),
            _ => None,
        }
    }

    /// Set the [valid_from, valid_to) interval; None clears a bound.
    /// Rejects an inverted interval (review P1-1): with both bounds set the
    /// kernel invariant is `valid_from <= valid_to`. Equality is a legal
    /// zero-duration interval — a claim closed at its own assertion instant,
    /// or a future fact invalidated before it ever became valid — and reads
    /// as valid_at nothing, which is exactly the intended policy.
    pub fn set_valid_time(&mut self, from: Option<u64>, to: Option<u64>) -> KResult<()> {
        if let (Some(f), Some(t)) = (from, to) {
            if f > t {
                return Err(KError::InvalidObject(format!(
                    "valid interval must satisfy valid_from <= valid_to (got {f} > {t})"
                )));
            }
        }
        match from {
            Some(f) => {
                self.extensions
                    .insert(Self::EXT_VALID_FROM.into(), Value::Int(f as i64));
            }
            None => {
                self.extensions.remove(Self::EXT_VALID_FROM);
            }
        }
        match to {
            Some(t) => {
                self.extensions
                    .insert(Self::EXT_VALID_TO.into(), Value::Int(t as i64));
            }
            None => {
                self.extensions.remove(Self::EXT_VALID_TO);
            }
        }
        Ok(())
    }

    /// Close an open validity interval at `at` (review P1-1 future-fact
    /// policy). A fact whose valid_from lies in the future (now < valid_from)
    /// must not gain valid_to < valid_from: that would invert the interval.
    /// Such an invalidation collapses to a zero-duration interval
    /// [valid_from, valid_from) — it was never valid, and it never becomes
    /// valid. No-op when the interval already has an end.
    pub fn close_valid_time(&mut self, at: u64) -> KResult<()> {
        if self.valid_to().is_some() {
            return Ok(());
        }
        let from = self.valid_from();
        let to = from.map_or(at, |f| f.max(at));
        self.set_valid_time(from, Some(to))
    }

    /// True when `at_millis` falls inside the validity interval. Half-open
    /// [valid_from, valid_to); an absent bound is unbounded on that side.
    pub fn valid_at(&self, at_millis: u64) -> bool {
        self.valid_from().map(|f| f <= at_millis).unwrap_or(true)
            && self.valid_to().map(|t| t > at_millis).unwrap_or(true)
    }

    /// Extension key for the verify-commit link (P2-5): the journal seq of
    /// the verify op's final commit — the event that carries the confidence
    /// bump this key lives beside. Kernel-managed (set by verify_knowledge,
    /// never by remember()).
    pub const EXT_VERIFIED_EVENT: &str = "verified_event";

    /// Journal seq of the most recent verify commit for this KO; None when
    /// never verified. Pairs with `last_verified` (the wall-clock instant) —
    /// this is the durable event in the audit journal that records it.
    pub fn verified_event(&self) -> Option<u64> {
        match self.extensions.get(Self::EXT_VERIFIED_EVENT) {
            Some(Value::Int(v)) if *v >= 0 => Some(*v as u64),
            _ => None,
        }
    }

    // ---- v0.3 K3: Derivation & confidence context (stored in extensions) ----

    /// Extension key for the Derivation record (Map; absent = asserted).
    pub const EXT_DERIVATION: &str = "derivation";
    /// Extension key for the ConfidenceContext record (Map).
    pub const EXT_CONFIDENCE: &str = "confidence";

    /// The first-class derivation record, if this KO was derived from others.
    /// Answers WHY (reason) / FROM WHAT (sources) / DERIVED HOW (operation,
    /// model) / BY WHOM (actor) / WHEN (timestamp) — a bare DERIVED_FROM
    /// edge is not enough (reviewer H4).
    pub fn derivation(&self) -> Option<Derivation> {
        self.extensions
            .get(Self::EXT_DERIVATION)
            .and_then(derivation_from_value)
    }

    /// Attach (or replace) the derivation record.
    pub fn set_derivation(&mut self, d: &Derivation) {
        self.extensions
            .insert(Self::EXT_DERIVATION.into(), derivation_to_value(d));
    }

    /// The confidence context, if set: score, independent confirmations,
    /// and when it was last verified.
    pub fn confidence_context(&self) -> Option<ConfidenceContext> {
        self.extensions
            .get(Self::EXT_CONFIDENCE)
            .and_then(confidence_from_value)
    }

    /// Attach (or replace) the confidence context.
    pub fn set_confidence_context(&mut self, c: &ConfidenceContext) {
        self.extensions
            .insert(Self::EXT_CONFIDENCE.into(), confidence_to_value(c));
    }

    // ---- v0.3 K4: Invalidation stamp (stored in extensions) ----

    /// Extension key for the Invalidation record (Map: at/actor/reason).
    /// Stamped when knowledge stops being supported — either directly
    /// (`invalidate` op) or by dependency propagation (a premise was
    /// superseded/invalidated). Distinct from Superseded/Contradicted:
    /// this records WHY support vanished, not just the resulting state.
    pub const EXT_INVALIDATION: &str = "invalidation";

    /// The invalidation record, if this KO's support was withdrawn.
    pub fn invalidation(&self) -> Option<Invalidation> {
        self.extensions
            .get(Self::EXT_INVALIDATION)
            .and_then(invalidation_from_value)
    }

    /// Attach (or replace) the invalidation record.
    pub fn set_invalidated(&mut self, at: u64, actor: &str, reason: &str) {
        let mut m = PropertyMap::new();
        m.insert("at".into(), Value::Int(at as i64));
        m.insert("actor".into(), Value::Text(actor.into()));
        m.insert("reason".into(), Value::Text(reason.into()));
        self.extensions
            .insert(Self::EXT_INVALIDATION.into(), Value::Map(m));
    }
}

// ---------------------------------------------------------------------------
// v0.3 K3: Derivation structure & confidence context model.
// Extension-backed (same locked pattern as K1/K2 state). KOIDs are encoded
// as hex Text; timestamp is epoch millis Int.
// ---------------------------------------------------------------------------

/// First-class derivation record: how this KO came to be.
#[derive(Clone, Debug, PartialEq)]
pub struct Derivation {
    /// The derivation operation (rule_fired, inference, merge, extraction…).
    pub operation: String,
    /// Who (or which agent) performed the derivation.
    pub actor: String,
    /// The model used, if the derivation was model-assisted.
    pub model: Option<String>,
    /// Epoch millis when the derivation happened.
    pub timestamp: u64,
    /// Premise KOs this object was derived from.
    pub sources: Vec<KOID>,
    /// Human-readable justification (the WHY).
    pub reason: Option<String>,
}

pub fn derivation_to_value(d: &Derivation) -> Value {
    let mut m = BTreeMap::new();
    m.insert("operation".into(), Value::Text(d.operation.clone()));
    m.insert("actor".into(), Value::Text(d.actor.clone()));
    if let Some(model) = &d.model {
        m.insert("model".into(), Value::Text(model.clone()));
    }
    m.insert("timestamp".into(), Value::Int(d.timestamp as i64));
    m.insert(
        "sources".into(),
        Value::List(d.sources.iter().map(|s| Value::Text(s.to_hex())).collect()),
    );
    if let Some(r) = &d.reason {
        m.insert("reason".into(), Value::Text(r.clone()));
    }
    Value::Map(m)
}

fn derivation_from_value(v: &Value) -> Option<Derivation> {
    let m = match v {
        Value::Map(m) => m,
        _ => return None,
    };
    let operation = match m.get("operation") {
        Some(Value::Text(s)) => s.clone(),
        _ => return None,
    };
    let actor = match m.get("actor") {
        Some(Value::Text(s)) => s.clone(),
        _ => return None,
    };
    let timestamp = match m.get("timestamp") {
        Some(Value::Int(t)) if *t >= 0 => *t as u64,
        _ => return None,
    };
    let sources = match m.get("sources") {
        Some(Value::List(l)) => {
            let mut srcs = Vec::new();
            for item in l {
                match item {
                    Value::Text(s) => match KOID::from_hex(s) {
                        Ok(k) => srcs.push(k),
                        Err(_) => return None,
                    },
                    _ => return None,
                }
            }
            srcs
        }
        _ => return None,
    };
    Some(Derivation {
        operation,
        actor,
        model: match m.get("model") {
            Some(Value::Text(s)) => Some(s.clone()),
            _ => None,
        },
        timestamp,
        sources,
        reason: match m.get("reason") {
            Some(Value::Text(s)) => Some(s.clone()),
            _ => None,
        },
    })
}

/// Confidence context model: how much the system trusts a KO, and why.
#[derive(Clone, Debug, PartialEq)]
pub struct ConfidenceContext {
    /// Aggregate confidence score (0.0–1.0).
    pub score: f32,
    /// Number of independent confirmations (review P2-4: distinct
    /// verifier+evidence keys only — re-verifying with the same evidence
    /// does not inflate the count).
    pub confirmations: u32,
    /// Epoch millis of the last verification, if any.
    pub last_verified: Option<u64>,
    /// Persisted confirmation keys backing `confirmations` (hash-like
    /// verifier|artifact|method|location|revision strings; absent = legacy
    /// record predating keyed confirmations).
    pub verification_keys: Vec<String>,
}

impl Default for ConfidenceContext {
    fn default() -> Self {
        ConfidenceContext {
            score: 0.0,
            confirmations: 0,
            last_verified: None,
            verification_keys: Vec::new(),
        }
    }
}

impl ConfidenceContext {
    /// The model boundary (review P1-7): every kernel construction site goes
    /// through here, so a NaN/±∞/out-of-range score is rejected up front —
    /// never clamped silently, never persisted.
    pub fn new(score: f32, confirmations: u32, last_verified: Option<u64>) -> KResult<Self> {
        if !score.is_finite() || !(0.0..=1.0).contains(&score) {
            return Err(KError::InvalidObject(format!(
                "confidence score must be finite and within [0,1], got {score}"
            )));
        }
        Ok(ConfidenceContext {
            score,
            confirmations,
            last_verified,
            verification_keys: Vec::new(),
        })
    }
}

pub fn confidence_to_value(c: &ConfidenceContext) -> Value {
    let mut m = BTreeMap::new();
    m.insert("score".into(), Value::Float(c.score as f64));
    m.insert("confirmations".into(), Value::Int(c.confirmations as i64));
    if let Some(v) = c.last_verified {
        m.insert("last_verified".into(), Value::Int(v as i64));
    }
    if !c.verification_keys.is_empty() {
        m.insert(
            "verification_keys".into(),
            Value::List(
                c.verification_keys
                    .iter()
                    .map(|k| Value::Text(k.clone()))
                    .collect(),
            ),
        );
    }
    Value::Map(m)
}

fn confidence_from_value(v: &Value) -> Option<ConfidenceContext> {
    let m = match v {
        Value::Map(m) => m,
        _ => return None,
    };
    let score = match m.get("score") {
        Some(Value::Float(f)) => *f as f32,
        Some(Value::Int(i)) => *i as f32,
        _ => return None,
    };
    // Reject a corrupt/non-finite score on decode too — it must not
    // roundtrip or feed ranking (review P1-7/P2-6).
    if !score.is_finite() || !(0.0..=1.0).contains(&score) {
        return None;
    }
    let confirmations = match m.get("confirmations") {
        Some(Value::Int(i)) if *i >= 0 => *i as u32,
        _ => return None,
    };
    let verification_keys = match m.get("verification_keys") {
        Some(Value::List(list)) => list
            .iter()
            .filter_map(|x| match x {
                Value::Text(s) => Some(s.clone()),
                _ => None,
            })
            .collect(),
        // Legacy records predate keyed confirmations (review P2-4 migration
        // note: their confirmations count is accepted as-is; the first new
        // verification adds its key and bumps by one).
        _ => Vec::new(),
    };
    Some(ConfidenceContext {
        score,
        confirmations,
        last_verified: match m.get("last_verified") {
            Some(Value::Int(t)) if *t >= 0 => Some(*t as u64),
            _ => None,
        },
        verification_keys,
    })
}

/// v0.3 K4: record of knowledge losing its support — either directly
/// (`invalidate` op) or propagated from an invalidated/superseded premise.
/// Distinct from the epistemic state: this is the WHY, the status is the WHAT.
#[derive(Clone, Debug, PartialEq)]
pub struct Invalidation {
    /// Epoch millis when the support was withdrawn.
    pub at: u64,
    /// Who withdrew the support.
    pub actor: String,
    /// Why the support was withdrawn.
    pub reason: String,
}

fn invalidation_from_value(v: &Value) -> Option<Invalidation> {
    let m = match v {
        Value::Map(m) => m,
        _ => return None,
    };
    Some(Invalidation {
        at: match m.get("at") {
            Some(Value::Int(t)) if *t >= 0 => *t as u64,
            _ => return None,
        },
        actor: match m.get("actor") {
            Some(Value::Text(s)) => s.clone(),
            _ => return None,
        },
        reason: match m.get("reason") {
            Some(Value::Text(s)) => s.clone(),
            _ => return None,
        },
    })
}

/// Canonical extension encoding of one evidence record (v0.3 K1).
fn evidence_to_value(ev: &crate::knowledge::evidence::Evidence) -> Value {
    let mut m = BTreeMap::new();
    m.insert(
        "source_artifact".into(),
        Value::Text(ev.source_artifact.clone()),
    );
    m.insert("method".into(), Value::Text(ev.method.as_str().into()));
    if let Some(l) = &ev.location {
        m.insert("location".into(), Value::Text(l.clone()));
    }
    if let Some(r) = &ev.revision {
        m.insert("revision".into(), Value::Text(r.clone()));
    }
    m.insert("confidence".into(), Value::Float(ev.confidence as f64));
    Value::Map(m)
}

// ---- R8 ContentTrust: trust level for ingested content ----

/// Trust level for content ingested from external sources.
/// Stored in `KnowledgeObject.extensions` under `EXT_CONTENT_TRUST`.
/// Used by secret filtering, prompt-injection guards, and audit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum ContentTrust {
    /// Authenticated system/admin input — highest trust.
    Trusted,
    /// External document/connector content — treat with caution.
    Untrusted,
    /// Trust level not explicitly set — conservative default (same as Untrusted).
    #[default]
    Unknown,
}

impl ContentTrust {
    pub fn as_str(&self) -> &str {
        match self {
            ContentTrust::Trusted => "trusted",
            ContentTrust::Untrusted => "untrusted",
            ContentTrust::Unknown => "unknown",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "trusted" => Some(ContentTrust::Trusted),
            "untrusted" => Some(ContentTrust::Untrusted),
            "unknown" => Some(ContentTrust::Unknown),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// v0.3 K1: Epistemic status — how the system knows a KO is true.
// Orthogonal to LifecycleState ("is it live?"); this is "how do we know?".
// Stored in extensions under EXT_EPISTEMIC_STATUS (Text) with an append-only
// transition history under EXT_EPISTEMIC_HISTORY (List of Maps).
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EpistemicStatus {
    /// Observed in the world — the conservative default for new KOs.
    Observed,
    /// Mechanically extracted from an artifact (parser, extractor).
    Extracted,
    /// Asserted by an agent or human.
    Asserted,
    /// Derived by reasoning from other knowledge.
    Inferred,
    /// Independently verified.
    Verified,
    /// Contradicted by other evidence.
    Contradicted,
    /// Replaced by newer knowledge.
    Superseded,
}

impl EpistemicStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            EpistemicStatus::Observed => "observed",
            EpistemicStatus::Extracted => "extracted",
            EpistemicStatus::Asserted => "asserted",
            EpistemicStatus::Inferred => "inferred",
            EpistemicStatus::Verified => "verified",
            EpistemicStatus::Contradicted => "contradicted",
            EpistemicStatus::Superseded => "superseded",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "observed" => Some(EpistemicStatus::Observed),
            "extracted" => Some(EpistemicStatus::Extracted),
            "asserted" => Some(EpistemicStatus::Asserted),
            "inferred" => Some(EpistemicStatus::Inferred),
            "verified" => Some(EpistemicStatus::Verified),
            "contradicted" => Some(EpistemicStatus::Contradicted),
            "superseded" => Some(EpistemicStatus::Superseded),
            _ => None,
        }
    }

    /// Constrained transition table (review H5 — deliberately NOT the
    /// review's illustrative linear chain: CONTRADICTED can hit any state
    /// directly, and INFERRED is a derivation origin, not a pipeline stage).
    /// Same-state transitions are rejected — every recorded transition must
    /// change state.
    pub fn can_transition(self, to: EpistemicStatus) -> bool {
        use EpistemicStatus::*;
        matches!(
            (self, to),
            (Observed, Extracted)
                | (Observed, Asserted)
                | (Observed, Verified)
                | (Observed, Contradicted)
                | (Observed, Superseded)
                | (Extracted, Asserted)
                | (Extracted, Verified)
                | (Extracted, Contradicted)
                | (Extracted, Superseded)
                | (Asserted, Verified)
                | (Asserted, Contradicted)
                | (Asserted, Superseded)
                | (Inferred, Verified)
                | (Inferred, Contradicted)
                | (Inferred, Superseded)
                | (Verified, Contradicted)
                | (Verified, Superseded)
            // A contradicted fact may be re-asserted on stronger evidence.
                | (Contradicted, Asserted)
                | (Contradicted, Superseded)
        )
    }

    /// Initial status for a create, by write origin.
    pub fn for_origin(origin: &Origin) -> Self {
        match origin {
            Origin::Reason => EpistemicStatus::Inferred,
            Origin::Human | Origin::Agent(_) => EpistemicStatus::Asserted,
            Origin::System | Origin::SemanticEnrichment => EpistemicStatus::Observed,
        }
    }
}

impl fmt::Display for EpistemicStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

// ---------------------------------------------------------------------------
// MRFC-0070 Phase A0: Canonical Relationship Types
// ---------------------------------------------------------------------------

/// Dependency relationship: source depends on target.
pub const DEPENDS_ON: &str = "depends_on";
/// Implementation relationship: source implements target (interface/contract).
pub const IMPLEMENTS: &str = "implements";
/// Test coverage: source is tested by target (test → implementation).
pub const TESTED_BY: &str = "tested_by";
/// Governance: source is governed by target (policy/rule).
pub const GOVERNED_BY: &str = "governed_by";
/// Documentation: source is documented by target.
pub const DOCUMENTED_BY: &str = "documented_by";
/// Constraint: source is constrained by target (rule/requirement).
pub const CONSTRAINED_BY: &str = "constrained_by";
/// Call graph: source calls target (function call).
pub const CALLS: &str = "calls";
/// Import: source imports target (module import).
pub const IMPORTS: &str = "imports";
/// Supersession: source supersedes target (newer version).
pub const SUPERSEDES: &str = "supersedes";
/// Contradiction: source contradicts target (conflicting claim).
pub const CONTRADICTS: &str = "contradicts";
/// Derivation: source is derived from target.
pub const DERIVED_FROM: &str = "derived_from";
/// Containment: source contains target (directory → file → entity).
pub const CONTAINS: &str = "contains";

/// All MRFC-0070 relationship types for iteration/discovery.
pub const RELATIONSHIP_TYPES: &[&str] = &[
    DEPENDS_ON,
    IMPLEMENTS,
    TESTED_BY,
    GOVERNED_BY,
    DOCUMENTED_BY,
    CONSTRAINED_BY,
    CALLS,
    IMPORTS,
    SUPERSEDES,
    CONTRADICTS,
    DERIVED_FROM,
    CONTAINS,
];

// ---------------------------------------------------------------------------
// MRFC-0070 Phase A0: Conflict Knowledge Object
// ---------------------------------------------------------------------------

/// A Conflict KO represents a contradiction between two Claims.
///
/// Created by `ConflictDetector` when two claims make contradictory statements
/// about the same subject. The Conflict has a resolution state machine:
///   Unresolved → UnderReview → Resolved (with resolution rationale).
#[derive(Clone, Debug, PartialEq)]
pub struct Conflict {
    /// KOID of the first contradictory claim.
    pub claim_a: KOID,
    /// KOID of the second contradictory claim.
    pub claim_b: KOID,
    /// Human-readable description of the contradiction.
    pub description: String,
    /// Resolution state.
    pub resolution: ConflictResolution,
    /// How the conflict was resolved (if resolved).
    pub resolution_rationale: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConflictResolution {
    /// Conflict has not been reviewed yet.
    Unresolved,
    /// Conflict is under active review.
    UnderReview,
    /// Claim A takes precedence over Claim B.
    ///
    /// Semantics (P2-1): a recorded selection of A as the current truth —
    /// B is transitioned to Contradicted with the mandatory rationale. It
    /// is a decision with a justification, never a strength ranking or an
    /// automatic win for the "stronger" side.
    ResolvedAPreferred,
    /// Claim B takes precedence over Claim A.
    ///
    /// Semantics (P2-1): the mirror of ResolvedAPreferred — A is
    /// transitioned to Contradicted with the mandatory rationale.
    ResolvedBPreferred,
    /// Both claims are valid in different contexts/scopes.
    ///
    /// Semantics (P2-1): a coexistence claim — neither side is demoted.
    /// `resolve_conflict` accepts an optional `split_at` instant that
    /// partitions validity along the valid-time axis (claim A valid until
    /// `split_at`, claim B valid from `split_at`), which is how "different
    /// contexts" is made queryable. Without `split_at` the resolution is a
    /// bare statement that both stand.
    ResolvedBothValid,
    /// Both claims rejected, replaced by a new claim.
    ///
    /// Semantics (P2-1): full supersession — both claims transition to
    /// Superseded with SUPERSEDES edges to the replacement, and derived
    /// dependents are swept for staleness.
    ResolvedReplaced,
}

impl ConflictResolution {
    pub fn as_str(self) -> &'static str {
        match self {
            ConflictResolution::Unresolved => "unresolved",
            ConflictResolution::UnderReview => "under_review",
            ConflictResolution::ResolvedAPreferred => "resolved_a_preferred",
            ConflictResolution::ResolvedBPreferred => "resolved_b_preferred",
            ConflictResolution::ResolvedBothValid => "resolved_both_valid",
            ConflictResolution::ResolvedReplaced => "resolved_replaced",
        }
    }

    pub fn is_resolved(self) -> bool {
        !matches!(
            self,
            ConflictResolution::Unresolved | ConflictResolution::UnderReview
        )
    }

    /// Parse the canonical wire form (as produced by `as_str`).
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "unresolved" => Some(ConflictResolution::Unresolved),
            "under_review" => Some(ConflictResolution::UnderReview),
            "resolved_a_preferred" => Some(ConflictResolution::ResolvedAPreferred),
            "resolved_b_preferred" => Some(ConflictResolution::ResolvedBPreferred),
            "resolved_both_valid" => Some(ConflictResolution::ResolvedBothValid),
            "resolved_replaced" => Some(ConflictResolution::ResolvedReplaced),
            _ => None,
        }
    }
}

/// Detects contradictory claims from two KOs.
///
/// ponytail: simple property-level comparison for now; full semantic
/// contradiction detection (e.g. via embedding similarity) is Phase A4.
pub struct ConflictDetector;

impl ConflictDetector {
    /// Detect conflicts between two Claim KOs by comparing their properties.
    /// Returns `Some(Conflict)` if the claims contradict each other,
    /// `None` otherwise.
    pub fn detect(claim_a_ko: &KnowledgeObject, claim_b_ko: &KnowledgeObject) -> Option<Conflict> {
        // Only compare Claim-typed KOs
        if claim_a_ko.metadata.type_name != "Claim" || claim_b_ko.metadata.type_name != "Claim" {
            return None;
        }
        // Same subject? Check "statement" and "subject" properties
        let subj_a = claim_a_ko
            .properties
            .get("subject")
            .or_else(|| claim_a_ko.properties.get("statement"));
        let subj_b = claim_b_ko
            .properties
            .get("subject")
            .or_else(|| claim_b_ko.properties.get("statement"));

        match (subj_a, subj_b) {
            (Some(Value::Text(a)), Some(Value::Text(b))) if a == b => Some(Conflict {
                claim_a: claim_a_ko.koid,
                claim_b: claim_b_ko.koid,
                description: format!("Contradictory claims about: {}", a),
                resolution: ConflictResolution::Unresolved,
                resolution_rationale: None,
            }),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Schema (MRFC-0001 §10 schema validation / INVALID_SCHEMA)
// ---------------------------------------------------------------------------

/// A lightweight schema definition against which KOs can be validated.
/// This is the Increment-1 subset: type, version, required property keys, and
/// optional closed-world allowed-property set. Future increments add property
/// types, relationship cardinality, and semantic constraints.
#[derive(Clone, Debug, PartialEq)]
pub struct Schema {
    pub type_name: String,
    pub schema_version: u32,
    pub required_properties: Vec<String>,
    /// If `Some`, the schema is "closed": any core property not listed here is
    /// treated as an unknown core field and rejected (MRFC-0001 §10).
    /// If `None`, unknown core properties are allowed (open-world default).
    pub allowed_properties: Option<HashSet<String>>,
    /// Typed property definitions. When non-empty, each property value is
    /// type-checked against its declared type during `SchemaRegistry::validate()`.
    /// MRFC-0060 Phase C1 — property type system.
    pub properties: Vec<SchemaProperty>,
    /// Uniqueness constraints (MRFC-0060 Phase C2).
    pub unique_constraints: Vec<UniqueConstraint>,
    /// Cross-property check constraints (MRFC-0060 Phase C4).
    pub check_constraints: Vec<CheckConstraint>,
}

/// An atomic schema migration: a new schema plus per-object property
/// transforms (EVO-003 apply/migrate op).
#[derive(Clone, Debug, PartialEq)]
pub struct SchemaMigration {
    pub schema: Schema,
    pub transforms: Vec<PropertyTransform>,
}

/// One per-object property rewrite applied during a schema migration.
#[derive(Clone, Debug, PartialEq)]
pub enum PropertyTransform {
    /// Move a property's value to a new key. Fails the migration if absent.
    Rename { from: String, to: String },
    /// Fill a missing property with a fixed value (existing values untouched).
    SetDefault { property: String, value: Value },
}

/// Outcome of `Kernel::apply_schema_migration`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MigrationReport {
    /// Live objects of the type examined.
    pub scanned: usize,
    /// Objects rewritten to the target schema version.
    pub migrated: usize,
    /// Objects already stamped with the target version (skipped).
    pub already_at_target: usize,
}

/// A typed property definition within a schema (MRFC-0060 Phase C1).
/// Separate from the ontology's `PropertyDef` — schema properties are the
/// enforced subset of what the ontology discovers.
#[derive(Clone, Debug, PartialEq)]
pub struct SchemaProperty {
    pub name: String,
    /// Expected aikoql value type: "Text", "Int", "Float", "Bool", "Bytes", "List", "Map".
    pub value_type: String,
    /// If true, the property must be present in every write.
    pub required: bool,
    /// If true, a Null value is accepted for this property.
    /// If false, any write of this property must carry a non-Null value of `value_type`.
    pub nullable: bool,
    /// If true, the property value must come from a trusted source (SemanticBlock.source).
    pub provenance_required: bool,
    /// Domain constraints applied to this property's value (MRFC-0060 Phase C4).
    pub domain_constraints: Vec<DomainConstraint>,
}

/// Uniqueness scope (MRFC-0060 Phase C2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UniquenessScope {
    /// Value must be unique within the type (same type_name).
    Type,
    /// Value must be unique within the tenant (all types in same tenant).
    Tenant,
    /// Value must be globally unique (all tenants and types).
    Global,
}

/// A uniqueness constraint on one or more properties (MRFC-0060 Phase C2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UniqueConstraint {
    /// Property names forming the unique key. Composite if len > 1.
    pub properties: Vec<String>,
    /// Scope of uniqueness enforcement.
    pub scope: UniquenessScope,
    /// When to evaluate this constraint (MRFC-0060 Phase C5).
    pub timing: ConstraintTiming,
}

// ---------------------------------------------------------------------------
// Domain + Check constraints (MRFC-0060 Phase C4)
// ---------------------------------------------------------------------------

/// Per-property value domain constraint.
#[derive(Clone, Debug, PartialEq)]
pub enum DomainConstraint {
    /// Numeric range [min, max] for Int/Float properties.
    Range { min: Option<f64>, max: Option<f64> },
    /// Glob-style pattern: `*` matches any sequence, `?` matches one char.
    Pattern(String),
    /// Length bounds for Text (chars) or Bytes.
    Length {
        min: Option<usize>,
        max: Option<usize>,
    },
    /// Value must be one of these exact values.
    Enum(Vec<Value>),
    /// Named format: "email", "url", "uuid", "date", "datetime".
    Format(String),
}

/// When a constraint is evaluated (MRFC-0060 Phase C5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ConstraintTiming {
    /// Evaluated at write time (per-statement).
    #[default]
    Immediate,
    /// Evaluated at commit time (end of transaction).
    Deferred,
}

/// A named check constraint with a boolean predicate expression.
#[derive(Clone, Debug, PartialEq)]
pub struct CheckConstraint {
    pub name: String,
    pub predicate: CheckExpression,
    /// When to evaluate this constraint (MRFC-0060 Phase C5).
    pub timing: ConstraintTiming,
}

/// Severity of a constraint violation (MRFC-0060 Phase C5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViolationSeverity {
    Error,
    Warning,
}

/// A single constraint violation (MRFC-0060 Phase C5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConstraintViolation {
    pub constraint_name: String,
    pub message: String,
    pub severity: ViolationSeverity,
    /// Commit timestamp when the violation was detected, 0 if immediate/pre-commit.
    pub timestamp: u64,
    /// KOID of the object that caused the violation, when attributable.
    pub koid: Option<KOID>,
}

impl ConstraintViolation {
    pub fn error(name: &str, msg: &str) -> Self {
        ConstraintViolation {
            constraint_name: name.into(),
            message: msg.into(),
            severity: ViolationSeverity::Error,
            timestamp: 0,
            koid: None,
        }
    }

    pub fn warning(name: &str, msg: &str) -> Self {
        ConstraintViolation {
            constraint_name: name.into(),
            message: msg.into(),
            severity: ViolationSeverity::Warning,
            timestamp: 0,
            koid: None,
        }
    }

    /// Set the koid on this violation (builder-style).
    pub fn with_koid(mut self, koid: KOID) -> Self {
        self.koid = Some(koid);
        self
    }
}

/// Result of a constraint evaluation pass (MRFC-0060 Phase C5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConstraintResult {
    pub valid: bool,
    pub violations: Vec<ConstraintViolation>,
    pub warnings: Vec<ConstraintViolation>,
}

impl ConstraintResult {
    pub fn ok() -> Self {
        ConstraintResult {
            valid: true,
            violations: Vec::new(),
            warnings: Vec::new(),
        }
    }

    pub fn fail(name: &str, msg: &str) -> Self {
        ConstraintResult {
            valid: false,
            violations: vec![ConstraintViolation::error(name, msg)],
            warnings: Vec::new(),
        }
    }

    /// Merge another result's violations and warnings into this one.
    pub fn merge(&mut self, other: &ConstraintResult) {
        if !other.valid {
            self.valid = false;
        }
        self.violations.extend(other.violations.clone());
        self.warnings.extend(other.warnings.clone());
    }

    /// Convert all errors and warnings into a single KError message.
    pub fn into_kresult(self) -> KResult<()> {
        if self.valid && self.warnings.is_empty() {
            return Ok(());
        }
        let mut parts: Vec<String> = Vec::new();
        for v in &self.violations {
            parts.push(format!("[{}] {}", v.constraint_name, v.message));
        }
        for w in &self.warnings {
            parts.push(format!("[WARN:{}] {}", w.constraint_name, w.message));
        }
        Err(KError::InvalidSchema(parts.join("; ")))
    }
}

/// A discovered constraint candidate from data inference (MRFC-0060 Phase C8).
/// Never auto-promoted to ENFORCED — caller reviews and manually registers
/// constraints via `register_schema()`.
#[derive(Clone, Debug)]
pub struct InferenceCandidate {
    /// Type this constraint applies to.
    pub type_name: String,
    /// Human-readable: "UNIQUE(email)", "NOT NULL age", "CHECK age BETWEEN 0 AND 150".
    pub constraint_desc: String,
    /// 0.0–1.0: 1.0 = no violations found in scanned data.
    pub confidence: f64,
    /// Total rows scanned.
    pub total_rows: usize,
    /// Number of rows that would violate this constraint.
    pub violations: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub enum CheckExpression {
    /// Reference to a property value: `@name`.
    Property(String),
    /// A literal value.
    Literal(Value),
    /// Binary comparison: `left op right`.
    Compare {
        op: CompareOp,
        left: Box<CheckExpression>,
        right: Box<CheckExpression>,
    },
    /// Logical AND.
    And(Box<CheckExpression>, Box<CheckExpression>),
    /// Logical OR.
    Or(Box<CheckExpression>, Box<CheckExpression>),
    /// Logical NOT.
    Not(Box<CheckExpression>),
    /// Arithmetic: left op right. Evaluates to a Value (MRFC-0060 Phase C9).
    Arith(Box<CheckExpression>, ArithOp, Box<CheckExpression>),
    /// Conditional: if condition is truthy, evaluate then branch, else else branch (C9).
    If(
        Box<CheckExpression>,
        Box<CheckExpression>,
        Box<CheckExpression>,
    ),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompareOp {
    Eq,
    Neq,
    Lt,
    Lte,
    Gt,
    Gte,
}

/// Arithmetic operators for `CheckExpression::Arith` (MRFC-0060 Phase C9).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArithOp {
    Add,
    Sub,
    Mul,
    Div,
}

impl Schema {
    pub fn new(type_name: &str, schema_version: u32) -> Self {
        Schema {
            type_name: type_name.into(),
            schema_version,
            required_properties: Vec::new(),
            allowed_properties: None,
            properties: Vec::new(),
            unique_constraints: Vec::new(),
            check_constraints: Vec::new(),
        }
    }

    pub fn require(mut self, prop: &str) -> Self {
        self.required_properties.push(prop.into());
        self
    }

    /// Enable closed-world validation: only the listed properties are permitted
    /// in the KO `properties` block. Automatically includes required properties.
    pub fn allow(mut self, prop: &str) -> Self {
        self.allowed_properties
            .get_or_insert_with(HashSet::new)
            .insert(prop.into());
        self
    }

    /// Add a typed property definition (MRFC-0060 Phase C1).
    pub fn property(mut self, name: &str, value_type: &str) -> Self {
        self.properties.push(SchemaProperty {
            name: name.into(),
            value_type: value_type.into(),
            required: false,
            nullable: false,
            provenance_required: false,
            domain_constraints: Vec::new(),
        });
        self
    }

    /// Add a typed property that is also required.
    pub fn required_property(mut self, name: &str, value_type: &str) -> Self {
        self.required_properties.push(name.into());
        self.properties.push(SchemaProperty {
            name: name.into(),
            value_type: value_type.into(),
            required: true,
            nullable: false,
            provenance_required: false,
            domain_constraints: Vec::new(),
        });
        self
    }

    /// Add a typed property that allows Null values.
    pub fn nullable_property(mut self, name: &str, value_type: &str) -> Self {
        self.properties.push(SchemaProperty {
            name: name.into(),
            value_type: value_type.into(),
            required: false,
            nullable: true,
            provenance_required: false,
            domain_constraints: Vec::new(),
        });
        self
    }

    /// Add a required, non-nullable property whose value must come from a trusted
    /// source (SemanticBlock.source must be present).  MRFC-0060 AC-17.
    pub fn provenance_required_property(mut self, name: &str, value_type: &str) -> Self {
        self.required_properties.push(name.into());
        self.properties.push(SchemaProperty {
            name: name.into(),
            value_type: value_type.into(),
            required: true,
            nullable: false,
            provenance_required: true,
            domain_constraints: Vec::new(),
        });
        self
    }

    /// Add a uniqueness constraint (MRFC-0060 Phase C2).
    pub fn unique(mut self, properties: &[&str], scope: UniquenessScope) -> Self {
        self.unique_constraints.push(UniqueConstraint {
            properties: properties.iter().map(|s| s.to_string()).collect(),
            scope,
            timing: ConstraintTiming::Immediate,
        });
        self
    }

    /// Add a uniqueness constraint evaluated at commit time (MRFC-0060 Phase C5).
    pub fn unique_deferred(mut self, properties: &[&str], scope: UniquenessScope) -> Self {
        self.unique_constraints.push(UniqueConstraint {
            properties: properties.iter().map(|s| s.to_string()).collect(),
            scope,
            timing: ConstraintTiming::Deferred,
        });
        self
    }

    /// Add a domain constraint to the most recently added property (MRFC-0060 Phase C4).
    pub fn domain_constraint(mut self, constraint: DomainConstraint) -> Self {
        if let Some(last) = self.properties.last_mut() {
            last.domain_constraints.push(constraint);
        }
        self
    }

    /// Add a cross-property check constraint evaluated immediately (MRFC-0060 Phase C4).
    pub fn check(mut self, name: &str, predicate: CheckExpression) -> Self {
        self.check_constraints.push(CheckConstraint {
            name: name.into(),
            predicate,
            timing: ConstraintTiming::Immediate,
        });
        self
    }

    /// Add a cross-property check constraint evaluated at commit time (MRFC-0060 Phase C5).
    pub fn check_deferred(mut self, name: &str, predicate: CheckExpression) -> Self {
        self.check_constraints.push(CheckConstraint {
            name: name.into(),
            predicate,
            timing: ConstraintTiming::Deferred,
        });
        self
    }

    /// Look up a property definition by name.
    pub fn find_property(&self, name: &str) -> Option<&SchemaProperty> {
        self.properties.iter().find(|p| p.name == name)
    }

    fn ensure_allowed_includes_required(&self) {
        if let Some(allowed) = &self.allowed_properties {
            for req in &self.required_properties {
                assert!(
                    allowed.contains(req),
                    "required property '{}' must also be allowed",
                    req
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Domain constraint validation (MRFC-0060 Phase C4)
// ---------------------------------------------------------------------------

impl DomainConstraint {
    /// Validate a property value against this domain constraint.
    pub fn validate(&self, value: &Value) -> Result<(), String> {
        match self {
            DomainConstraint::Range { min, max } => {
                let n = match value {
                    Value::Int(i) => *i as f64,
                    Value::Float(f) => *f,
                    other => {
                        return Err(format!(
                            "Range constraint requires numeric value, got {}",
                            other.type_name()
                        ));
                    }
                };
                if let Some(min) = min {
                    if n < *min {
                        return Err(format!("value {} is below minimum {}", n, min));
                    }
                }
                if let Some(max) = max {
                    if n > *max {
                        return Err(format!("value {} exceeds maximum {}", n, max));
                    }
                }
                Ok(())
            }
            DomainConstraint::Pattern(pattern) => match value {
                Value::Text(s) => {
                    let re = Regex::new(pattern)
                        .map_err(|e| format!("invalid regex pattern '{}': {}", pattern, e))?;
                    if re.is_match(s) {
                        Ok(())
                    } else {
                        Err(format!(
                            "value '{}' does not match pattern '{}'",
                            s, pattern
                        ))
                    }
                }
                other => Err(format!(
                    "Pattern constraint requires Text value, got {}",
                    other.type_name()
                )),
            },
            DomainConstraint::Length { min, max } => {
                let len = match value {
                    Value::Text(s) => s.len(),
                    Value::Bytes(b) => b.len(),
                    other => {
                        return Err(format!(
                            "Length constraint requires Text or Bytes, got {}",
                            other.type_name()
                        ));
                    }
                };
                if let Some(min) = min {
                    if len < *min {
                        return Err(format!("length {} is below minimum {}", len, min));
                    }
                }
                if let Some(max) = max {
                    if len > *max {
                        return Err(format!("length {} exceeds maximum {}", len, max));
                    }
                }
                Ok(())
            }
            DomainConstraint::Enum(values) => {
                if values.contains(value) {
                    Ok(())
                } else {
                    Err("value is not one of the allowed enum values".into())
                }
            }
            DomainConstraint::Format(format) => validate_format(format, value),
        }
    }
}

fn validate_format(format: &str, value: &Value) -> Result<(), String> {
    let s = match value {
        Value::Text(s) => s.as_str(),
        other => {
            return Err(format!(
                "Format constraint requires Text, got {}",
                other.type_name()
            ));
        }
    };
    match format {
        "email" => {
            // RFC 5321 simplified: local@domain
            // justified: compile-time literal pattern — a failure here is a code bug
            let re = Regex::new(r"^[a-zA-Z0-9._%+-]+@[a-zA-Z0-9.-]+\.[a-zA-Z]{2,}$").unwrap();
            if re.is_match(s) {
                Ok(())
            } else {
                Err(format!("'{}' is not a valid email", s))
            }
        }
        "url" => {
            // justified: compile-time literal pattern — a failure here is a code bug
            let re = Regex::new(r"^https?://[a-zA-Z0-9.-]+(:\d+)?(/[^\s]*)?$").unwrap();
            if re.is_match(s) {
                Ok(())
            } else {
                Err(format!("'{}' is not a valid URL", s))
            }
        }
        "uuid" => {
            let re = Regex::new(
                r"^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$",
            )
            // justified: compile-time literal pattern — a failure here is a code bug
            .unwrap();
            if re.is_match(s) {
                Ok(())
            } else {
                Err(format!("'{}' is not a valid UUID", s))
            }
        }
        "date" => {
            // YYYY-MM-DD with valid month (01-12) and day (01-31).
            // justified: compile-time literal pattern — a failure here is a code bug
            let re = Regex::new(r"^(\d{4})-(\d{2})-(\d{2})$").unwrap();
            if let Some(caps) = re.captures(s) {
                let month: u32 = caps[2].parse().unwrap_or(0);
                let day: u32 = caps[3].parse().unwrap_or(0);
                if (1..=12).contains(&month) && (1..=31).contains(&day) {
                    return Ok(());
                }
            }
            Err(format!("'{}' is not a valid date (YYYY-MM-DD)", s))
        }
        "datetime" => {
            // ISO 8601: YYYY-MM-DDTHH:MM:SS[.fff][Z|±HH:MM]
            let re =
                Regex::new(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d+)?(Z|[+-]\d{2}:\d{2})?$")
                    // justified: compile-time literal pattern — a failure here is a code bug
                    .unwrap();
            if re.is_match(s) {
                Ok(())
            } else {
                Err(format!("'{}' is not a valid datetime (ISO 8601)", s))
            }
        }
        _ => Err(format!("unknown format: '{}'", format)),
    }
}

// ---------------------------------------------------------------------------
// Check expression evaluation (MRFC-0060 Phase C4)
// ---------------------------------------------------------------------------

impl CheckExpression {
    /// Evaluate this expression against a property map, returning a boolean result.
    pub fn evaluate(&self, props: &PropertyMap) -> Result<bool, String> {
        match self {
            CheckExpression::Property(name) => props
                .get(name)
                .map(is_truthy)
                .ok_or_else(|| format!("property '{}' not found", name)),
            CheckExpression::Literal(v) => Ok(is_truthy(v)),
            CheckExpression::Compare { op, left, right } => {
                let l = left.eval_value(props)?;
                let r = right.eval_value(props)?;
                compare_values(*op, &l, &r)
            }
            CheckExpression::And(l, r) => Ok(l.evaluate(props)? && r.evaluate(props)?),
            CheckExpression::Or(l, r) => Ok(l.evaluate(props)? || r.evaluate(props)?),
            CheckExpression::Not(e) => Ok(!e.evaluate(props)?),
            CheckExpression::Arith(..) => {
                let v = self.eval_value(props)?;
                Ok(is_truthy(&v))
            }
            CheckExpression::If(cond, then_expr, else_expr) => {
                if cond.evaluate(props)? {
                    then_expr.evaluate(props)
                } else {
                    else_expr.evaluate(props)
                }
            }
        }
    }

    /// Return all property names referenced in this expression tree.
    /// Used by C6 write-set filtering to skip unaffected check constraints.
    pub fn referenced_properties(&self) -> Vec<&str> {
        let mut out = Vec::new();
        self.collect_refs(&mut out);
        out
    }

    fn collect_refs<'a>(&'a self, out: &mut Vec<&'a str>) {
        match self {
            CheckExpression::Property(name) => out.push(name),
            CheckExpression::Literal(_) => {}
            CheckExpression::Compare { left, right, .. } => {
                left.collect_refs(out);
                right.collect_refs(out);
            }
            CheckExpression::And(l, r) | CheckExpression::Or(l, r) => {
                l.collect_refs(out);
                r.collect_refs(out);
            }
            CheckExpression::Not(inner) => inner.collect_refs(out),
            CheckExpression::Arith(l, _, r) => {
                l.collect_refs(out);
                r.collect_refs(out);
            }
            CheckExpression::If(cond, then_expr, else_expr) => {
                cond.collect_refs(out);
                then_expr.collect_refs(out);
                else_expr.collect_refs(out);
            }
        }
    }

    /// Evaluate to a Value (for comparison operands and arithmetic).
    fn eval_value(&self, props: &PropertyMap) -> Result<Value, String> {
        match self {
            CheckExpression::Property(name) => props
                .get(name)
                .cloned()
                .ok_or_else(|| format!("property '{}' not found", name)),
            CheckExpression::Literal(v) => Ok(v.clone()),
            CheckExpression::Arith(_, op, _) => {
                // Extract operands via recursive matching for borrowck.
                let (left, right) = match self {
                    CheckExpression::Arith(l, _, r) => (l, r),
                    _ => unreachable!(),
                };
                let l = left.eval_value(props)?;
                let r = right.eval_value(props)?;
                arith_values(*op, &l, &r)
            }
            CheckExpression::If(_, _, _) => {
                let (cond, then_expr, else_expr) = match self {
                    CheckExpression::If(c, t, e) => (c, t, e),
                    _ => unreachable!(),
                };
                if cond.evaluate(props)? {
                    then_expr.eval_value(props)
                } else {
                    else_expr.eval_value(props)
                }
            }
            _ => Err("expected value expression, got logical or comparison".into()),
        }
    }
}

/// Arithmetic evaluation: apply `op` to two `Value`s (MRFC-0060 Phase C9).
fn arith_values(op: ArithOp, left: &Value, right: &Value) -> Result<Value, String> {
    match op {
        ArithOp::Add => match (left, right) {
            (Value::Int(l), Value::Int(r)) => Ok(Value::Int(l + r)),
            (Value::Int(l), Value::Float(r)) => Ok(Value::Float(*l as f64 + r)),
            (Value::Float(l), Value::Int(r)) => Ok(Value::Float(l + *r as f64)),
            (Value::Float(l), Value::Float(r)) => Ok(Value::Float(l + r)),
            (Value::Text(l), Value::Text(r)) => Ok(Value::Text(format!("{}{}", l, r))),
            _ => Err(format!(
                "cannot add {:?} and {:?}",
                left.type_name(),
                right.type_name()
            )),
        },
        ArithOp::Sub => match (left, right) {
            (Value::Int(l), Value::Int(r)) => Ok(Value::Int(l - r)),
            (Value::Int(l), Value::Float(r)) => Ok(Value::Float(*l as f64 - r)),
            (Value::Float(l), Value::Int(r)) => Ok(Value::Float(l - *r as f64)),
            (Value::Float(l), Value::Float(r)) => Ok(Value::Float(l - r)),
            _ => Err(format!(
                "cannot subtract {:?} and {:?}",
                left.type_name(),
                right.type_name()
            )),
        },
        ArithOp::Mul => match (left, right) {
            (Value::Int(l), Value::Int(r)) => Ok(Value::Int(l * r)),
            (Value::Int(l), Value::Float(r)) => Ok(Value::Float(*l as f64 * r)),
            (Value::Float(l), Value::Int(r)) => Ok(Value::Float(l * *r as f64)),
            (Value::Float(l), Value::Float(r)) => Ok(Value::Float(l * r)),
            _ => Err(format!(
                "cannot multiply {:?} and {:?}",
                left.type_name(),
                right.type_name()
            )),
        },
        ArithOp::Div => {
            // Check for division by zero before matching types.
            let is_div_zero = match right {
                Value::Int(0) => true,
                Value::Float(f) if *f == 0.0 => true,
                _ => false,
            };
            if is_div_zero {
                return Err("division by zero".into());
            }
            match (left, right) {
                (Value::Int(l), Value::Int(r)) => Ok(Value::Int(l / r)),
                (Value::Int(l), Value::Float(r)) => Ok(Value::Float(*l as f64 / r)),
                (Value::Float(l), Value::Int(r)) => Ok(Value::Float(l / *r as f64)),
                (Value::Float(l), Value::Float(r)) => Ok(Value::Float(l / r)),
                _ => Err(format!(
                    "cannot divide {:?} and {:?}",
                    left.type_name(),
                    right.type_name()
                )),
            }
        }
    }
}

fn is_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Int(0) => false,
        Value::Int(_) => true,
        Value::Float(f) if *f == 0.0 => false,
        Value::Float(_) => true,
        Value::Text(s) => !s.is_empty(),
        Value::Bytes(b) => !b.is_empty(),
        Value::List(l) => !l.is_empty(),
        Value::Map(m) => !m.is_empty(),
    }
}

fn compare_values(op: CompareOp, left: &Value, right: &Value) -> Result<bool, String> {
    match op {
        CompareOp::Eq => Ok(left == right),
        CompareOp::Neq => Ok(left != right),
        CompareOp::Lt | CompareOp::Lte | CompareOp::Gt | CompareOp::Gte => match (left, right) {
            (Value::Int(l), Value::Int(r)) => compare_ordered(op, *l, *r),
            (Value::Int(l), Value::Float(r)) => compare_ordered(op, *l as f64, *r),
            (Value::Float(l), Value::Int(r)) => compare_ordered(op, *l, *r as f64),
            (Value::Float(l), Value::Float(r)) => compare_ordered(op, *l, *r),
            (Value::Text(l), Value::Text(r)) => compare_ordered(op, l.as_str(), r.as_str()),
            _ => Err(format!(
                "cannot compare {} with {}",
                left.type_name(),
                right.type_name()
            )),
        },
    }
}

fn compare_ordered<T: PartialOrd>(op: CompareOp, l: T, r: T) -> Result<bool, String> {
    match op {
        CompareOp::Lt => Ok(l < r),
        CompareOp::Lte => Ok(l <= r),
        CompareOp::Gt => Ok(l > r),
        CompareOp::Gte => Ok(l >= r),
        _ => unreachable!(),
    }
}

// ---------------------------------------------------------------------------
// Expression string parser (MRFC-0060 Phase C4)
// ---------------------------------------------------------------------------

/// Token produced by the expression tokenizer.
#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Ident(String),
    Str(String),
    Int(i64),
    Float(f64),
    EqEq,
    NotEq,
    LtEq,
    GtEq,
    Lt,
    Gt,
    LParen,
    RParen,
    At,
}

struct Parser {
    tokens: Vec<Tok>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.tokens.get(self.pos)
    }

    fn next(&mut self) -> Result<Tok, String> {
        self.tokens
            .get(self.pos)
            .cloned()
            .ok_or_else(|| "unexpected end of expression".into())
            .inspect(|_t| {
                self.pos += 1;
            })
    }

    fn eat_kw(&mut self, kw: &str) -> bool {
        if let Some(Tok::Ident(s)) = self.peek() {
            if s.eq_ignore_ascii_case(kw) {
                self.pos += 1;
                return true;
            }
        }
        false
    }

    fn eat_op(&mut self) -> Option<CompareOp> {
        match self.peek()? {
            Tok::EqEq => {
                self.pos += 1;
                Some(CompareOp::Eq)
            }
            Tok::NotEq => {
                self.pos += 1;
                Some(CompareOp::Neq)
            }
            Tok::LtEq => {
                self.pos += 1;
                Some(CompareOp::Lte)
            }
            Tok::GtEq => {
                self.pos += 1;
                Some(CompareOp::Gte)
            }
            Tok::Lt => {
                self.pos += 1;
                Some(CompareOp::Lt)
            }
            Tok::Gt => {
                self.pos += 1;
                Some(CompareOp::Gt)
            }
            _ => None,
        }
    }

    // expr = or_expr
    fn parse_expr(&mut self) -> Result<CheckExpression, String> {
        self.parse_or()
    }

    // or_expr = and_expr ("OR" and_expr)*
    fn parse_or(&mut self) -> Result<CheckExpression, String> {
        let mut left = self.parse_and()?;
        while self.eat_kw("OR") {
            let right = self.parse_and()?;
            left = CheckExpression::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    // and_expr = not_expr ("AND" not_expr)*
    fn parse_and(&mut self) -> Result<CheckExpression, String> {
        let mut left = self.parse_not()?;
        while self.eat_kw("AND") {
            let right = self.parse_not()?;
            left = CheckExpression::And(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    // not_expr = "NOT" not_expr | comparison
    fn parse_not(&mut self) -> Result<CheckExpression, String> {
        if self.eat_kw("NOT") {
            let inner = self.parse_not()?;
            return Ok(CheckExpression::Not(Box::new(inner)));
        }
        self.parse_comparison()
    }

    // comparison = value (cmp_op value)?
    fn parse_comparison(&mut self) -> Result<CheckExpression, String> {
        let left = self.parse_value()?;
        if let Some(op) = self.eat_op() {
            let right = self.parse_value()?;
            return Ok(CheckExpression::Compare {
                op,
                left: Box::new(left),
                right: Box::new(right),
            });
        }
        Ok(left)
    }

    // value = "(" expr ")" | "@" identifier | literal | identifier
    fn parse_value(&mut self) -> Result<CheckExpression, String> {
        // "@property" syntax
        if matches!(self.peek(), Some(Tok::At)) {
            self.pos += 1; // skip @
            let name = match self.next()? {
                Tok::Ident(s) => s,
                t => return Err(format!("expected property name after '@', got {:?}", t)),
            };
            return Ok(CheckExpression::Property(name));
        }
        match self.next()? {
            Tok::LParen => {
                let expr = self.parse_expr()?;
                match self.next()? {
                    Tok::RParen => Ok(expr),
                    t => Err(format!("expected ')', got {:?}", t)),
                }
            }
            Tok::Ident(s) => {
                // Keywords as literal values
                match s.to_uppercase().as_str() {
                    "TRUE" => return Ok(CheckExpression::Literal(Value::Bool(true))),
                    "FALSE" => return Ok(CheckExpression::Literal(Value::Bool(false))),
                    "NULL" => return Ok(CheckExpression::Literal(Value::Null)),
                    _ => {}
                }
                Ok(CheckExpression::Property(s))
            }
            Tok::Str(s) => Ok(CheckExpression::Literal(Value::Text(s))),
            Tok::Int(n) => Ok(CheckExpression::Literal(Value::Int(n))),
            Tok::Float(f) => Ok(CheckExpression::Literal(Value::Float(f))),
            t => Err(format!("unexpected token: {:?}", t)),
        }
    }
}

/// Tokenize an expression string.
fn tokenize(input: &str) -> Result<Vec<Tok>, String> {
    let mut tokens = Vec::new();
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0usize;
    while i < chars.len() {
        let c = chars[i];
        // Whitespace
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        // Two-character operators (and single = as Eq alias)
        if i + 1 < chars.len() {
            let two: String = chars[i..=i + 1].iter().collect();
            match two.as_str() {
                "==" => {
                    tokens.push(Tok::EqEq);
                    i += 2;
                    continue;
                }
                "!=" => {
                    tokens.push(Tok::NotEq);
                    i += 2;
                    continue;
                }
                "<=" => {
                    tokens.push(Tok::LtEq);
                    i += 2;
                    continue;
                }
                ">=" => {
                    tokens.push(Tok::GtEq);
                    i += 2;
                    continue;
                }
                _ => {}
            }
        }
        // Single '=' as equality alias
        if c == '=' {
            tokens.push(Tok::EqEq);
            i += 1;
            continue;
        }
        // Single-character tokens
        match c {
            '(' => {
                tokens.push(Tok::LParen);
                i += 1;
                continue;
            }
            ')' => {
                tokens.push(Tok::RParen);
                i += 1;
                continue;
            }
            '<' => {
                tokens.push(Tok::Lt);
                i += 1;
                continue;
            }
            '>' => {
                tokens.push(Tok::Gt);
                i += 1;
                continue;
            }
            '@' => {
                tokens.push(Tok::At);
                i += 1;
                continue;
            }
            '"' | '\'' => {
                let quote = c;
                i += 1; // skip opening quote
                let start = i;
                while i < chars.len() && chars[i] != quote {
                    if chars[i] == '\\' && i + 1 < chars.len() {
                        i += 1; // skip escape
                    }
                    i += 1;
                }
                if i >= chars.len() {
                    return Err("unterminated string literal".into());
                }
                let s: String = chars[start..i].iter().collect();
                // Unescape basic sequences
                let s = s
                    .replace("\\\"", "\"")
                    .replace("\\'", "'")
                    .replace("\\\\", "\\");
                tokens.push(Tok::Str(s));
                i += 1; // skip closing quote
                continue;
            }
            _ => {}
        }
        // Number literals
        if c == '-' || c.is_ascii_digit() {
            let start = i;
            if c == '-' {
                i += 1;
            }
            let mut is_float = false;
            while i < chars.len() && chars[i].is_ascii_digit() {
                i += 1;
            }
            if i < chars.len() && chars[i] == '.' {
                is_float = true;
                i += 1;
                while i < chars.len() && chars[i].is_ascii_digit() {
                    i += 1;
                }
            }
            let num_str: String = chars[start..i].iter().collect();
            if is_float {
                let val: f64 = num_str
                    .parse()
                    .map_err(|_| format!("invalid float: {}", num_str))?;
                tokens.push(Tok::Float(val));
            } else {
                let val: i64 = num_str
                    .parse()
                    .map_err(|_| format!("invalid integer: {}", num_str))?;
                tokens.push(Tok::Int(val));
            }
            continue;
        }
        // Identifiers
        if c.is_alphabetic() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            let ident: String = chars[start..i].iter().collect();
            tokens.push(Tok::Ident(ident));
            continue;
        }
        return Err(format!("unexpected character: '{}' at position {}", c, i));
    }
    Ok(tokens)
}

impl CheckExpression {
    /// Parse an expression string into a `CheckExpression` AST.
    ///
    /// Grammar:
    /// ```text
    /// expr     = or_expr
    /// or_expr  = and_expr ("OR" and_expr)*
    /// and_expr = not_expr ("AND" not_expr)*
    /// not_expr = "NOT" not_expr | comparison
    /// comparison = value (cmp_op value)?
    /// value    = "(" expr ")" | "@" identifier | literal | identifier
    /// literal  = STRING | NUMBER | "true" | "false" | "null"
    /// cmp_op   = "==" | "!=" | "<=" | ">=" | "<" | ">"
    /// ```
    ///
    /// # Examples
    ///
    /// ```rust
    /// use aikoql_kernel::knowledge::kom::CheckExpression;
    ///
    /// let _ = CheckExpression::parse("end_date >= start_date").unwrap();
    /// let _ = CheckExpression::parse("age >= 18 AND age <= 120").unwrap();
    /// let _ = CheckExpression::parse("NOT (status = \"deleted\")").unwrap();
    /// ```
    pub fn parse(input: &str) -> Result<CheckExpression, String> {
        let tokens = tokenize(input)?;
        let mut parser = Parser { tokens, pos: 0 };
        let expr = parser.parse_expr()?;
        if parser.pos < parser.tokens.len() {
            return Err(format!(
                "unexpected token after expression: {:?}",
                parser.tokens[parser.pos]
            ));
        }
        Ok(expr)
    }
}

// ---------------------------------------------------------------------------
// Public trait (MRFC-0001 §9)
// ---------------------------------------------------------------------------

/// Canonical abstraction exposed by every KOM implementation.
pub trait KnowledgeEntity {
    fn id(&self) -> KOID;
    fn metadata(&self) -> &Metadata;
    fn properties(&self) -> &PropertyMap;
    fn relationships(&self) -> &[RelationshipRef];
    fn events(&self) -> &[EventRef];
    fn security(&self) -> &SecurityDescriptor;
    fn semantic(&self) -> Option<&SemanticBlock>;
}

impl KnowledgeEntity for KnowledgeObject {
    fn id(&self) -> KOID {
        self.koid
    }
    fn metadata(&self) -> &Metadata {
        &self.metadata
    }
    fn properties(&self) -> &PropertyMap {
        &self.properties
    }
    fn relationships(&self) -> &[RelationshipRef] {
        &self.relationships
    }
    fn events(&self) -> &[EventRef] {
        &self.event_refs
    }
    fn security(&self) -> &SecurityDescriptor {
        &self.security
    }
    fn semantic(&self) -> Option<&SemanticBlock> {
        self.semantic.as_ref()
    }
}

// ---------------------------------------------------------------------------
// Knowledge Event (append-only journal entry, MRFC-0001 §4 req 5)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct KnowledgeEvent {
    /// Journal sequence number (monotone, gapless per kernel).
    pub seq: u64,
    pub koid: KOID,
    pub version: u64,
    pub kind: EventKind,
    pub origin: Origin,
    pub actor: String,
    pub commit_ts: u64,
    /// SHA-256 of the committed object payload. Protects audit integrity
    /// per MRFC-0001 §12 (replaces the earlier FNV-1a-64 placeholder).
    pub payload_hash: [u8; 32],
    /// Hash-chain links for tamper evidence.
    pub prev_audit_hash: [u8; 32],
    pub audit_hash: [u8; 32],
    /// Optional HMAC-SHA256 signature of the payload. Enabled when the kernel
    /// is opened with a signing key; proves payload integrity independently of
    /// the audit chain (MRFC-0011 §6.7).
    pub signature: Option<[u8; 32]>,
    pub note: Option<String>,
}

// ---------------------------------------------------------------------------
// Error model (MRFC-0001 §11 + MRFC-0011 §8)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub enum KError {
    InvalidObject(String),
    InvalidSchema(String),
    InvalidQuery(String),
    VersionConflict {
        koid: KOID,
        expected: u64,
        found: u64,
    },
    AccessDenied {
        subject: String,
        action: Action,
        koid: KOID,
    },
    InvalidState {
        from: LifecycleState,
        to: LifecycleState,
    },
    /// v0.3 K1: illegal epistemic status transition.
    InvalidEpistemic {
        from: EpistemicStatus,
        to: EpistemicStatus,
    },
    NotFound(KOID),
    UnsupportedOperation(String),
    IndexLagExceeded,
    JobRejected(String),
    Store(String),
    Codec(String),
}

impl fmt::Display for KError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KError::InvalidObject(m) => write!(f, "INVALID_OBJECT: {}", m),
            KError::InvalidSchema(m) => write!(f, "INVALID_SCHEMA: {}", m),
            KError::InvalidQuery(m) => write!(f, "INVALID_QUERY: {}", m),
            KError::VersionConflict {
                koid,
                expected,
                found,
            } => write!(
                f,
                "VERSION_CONFLICT: {} expected version {} found {}",
                koid, expected, found
            ),
            KError::AccessDenied {
                subject,
                action,
                koid,
            } => {
                write!(f, "ACCESS_DENIED: {} cannot {} {}", subject, action, koid)
            }
            KError::InvalidState { from, to } => {
                write!(f, "INVALID_STATE: {} -> {}", from, to)
            }
            KError::InvalidEpistemic { from, to } => {
                write!(f, "INVALID_EPISTEMIC: {} -> {}", from, to)
            }
            KError::NotFound(k) => write!(f, "NOT_FOUND: {}", k),
            KError::UnsupportedOperation(m) => write!(f, "UNSUPPORTED_OPERATION: {}", m),
            KError::IndexLagExceeded => write!(f, "INDEX_LAG_EXCEEDED"),
            KError::JobRejected(m) => write!(f, "JOB_REJECTED: {}", m),
            KError::Store(m) => write!(f, "STORE: {}", m),
            KError::Codec(m) => write!(f, "CODEC: {}", m),
        }
    }
}

impl std::error::Error for KError {}

pub type KResult<T> = Result<T, KError>;

// ---------------------------------------------------------------------------
// Deterministic hashes
// ---------------------------------------------------------------------------

/// FNV-1a 64-bit. Deterministic across platforms and processes.
/// Retained for non-audit use cases; the audit stream uses SHA-256.
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// SHA-256 integrity hash. Used for audit-chain integrity (MRFC-0001 §12).
pub fn sha256(bytes: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().into()
}

/// HMAC-SHA256 keyed signature. Used for at-rest version signatures
/// when a signing key is configured (MRFC-0011 §6.7).
pub fn hmac_sha256(key: &[u8; 32], bytes: &[u8]) -> [u8; 32] {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    type HmacSha256 = Hmac<Sha256>;
    // justified: HMAC-SHA256 accepts any key length; key is fixed [u8; 32]
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(bytes);
    mac.finalize().into_bytes().into()
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_legal_transitions() {
        use LifecycleState::*;
        let legal = [
            (Draft, Active),
            (Active, Verified),
            (Verified, Archived),
            (Archived, Deleted),
        ];
        for (a, b) in legal {
            assert!(a.can_transition(b), "{} -> {} must be legal", a, b);
        }
    }

    #[test]
    fn lifecycle_illegal_transitions() {
        use LifecycleState::*;
        let states = [Draft, Active, Verified, Archived, Deleted];
        for from in states {
            for to in states {
                let legal = matches!(
                    (from, to),
                    (Draft, Active)
                        | (Active, Verified)
                        | (Verified, Archived)
                        | (Archived, Deleted)
                );
                assert_eq!(from.can_transition(to), legal, "{} -> {}", from, to);
            }
        }
    }

    #[test]
    fn idgen_is_monotonic_and_unique() {
        let mut g = IdGen::new(7);
        let a = g.next(1000);
        let b = g.next(1000); // same ms -> counter bump
        let c = g.next(1001);
        assert!(a < b);
        assert!(b < c);
        assert_ne!(a, b);
        // clock going backwards must not reuse ids
        let d = g.next(5);
        assert!(c < d);
    }

    #[test]
    fn idgen_salts_diverge() {
        let mut g1 = IdGen::new(1);
        let mut g2 = IdGen::new(2);
        assert_ne!(g1.next(1000), g2.next(1000));
    }

    #[test]
    fn fnv_known_vectors() {
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a64(b"aikoql"), fnv1a64(b"aikoql"));
        assert_ne!(fnv1a64(b"a"), fnv1a64(b"b"));
    }

    #[test]
    fn validate_rejects_empty_acl_principal() {
        let mut ko = KnowledgeObject::new(
            KOID::ZERO,
            Metadata {
                type_name: "fact".into(),
                tenant: None,
                schema_version: 1,
                tags: vec![],
            },
            SecurityDescriptor {
                owner: "alice".into(),
                acl: vec![AclEntry {
                    principal: "  ".into(),
                    action: Action::Read,
                    effect: Effect::Allow,
                }],
                classification: None,
            },
        );
        assert!(matches!(ko.validate(), Err(KError::InvalidObject(_))));
        ko.security.acl[0].principal = "bob".into();
        assert!(ko.validate().is_ok());
    }

    #[test]
    fn new_ko_passes_basic_mandatory_validation() {
        let ko = KnowledgeObject::new(
            KOID::ZERO,
            Metadata {
                type_name: "fact".into(),
                tenant: None,
                schema_version: 1,
                tags: vec![],
            },
            SecurityDescriptor {
                owner: "alice".into(),
                acl: vec![],
                classification: None,
            },
        );
        assert!(ko.validate().is_ok());
    }

    #[test]
    fn knowledge_entity_trait_exposes_all_blocks() {
        use super::KnowledgeEntity;
        let ko = KnowledgeObject::new(
            KOID::ZERO,
            Metadata {
                type_name: "fact".into(),
                tenant: None,
                schema_version: 1,
                tags: vec![],
            },
            SecurityDescriptor {
                owner: "alice".into(),
                acl: vec![],
                classification: None,
            },
        );
        assert_eq!(ko.id(), KOID::ZERO);
        assert_eq!(ko.metadata().type_name, "fact");
        assert!(ko.properties().is_empty());
        assert!(ko.relationships().is_empty());
        assert!(ko.events().is_empty());
        assert_eq!(ko.security().owner, "alice");
        assert!(ko.semantic().is_none());
    }

    #[test]
    fn validate_against_schema_rejects_type_mismatch() {
        let ko = KnowledgeObject::new(
            KOID::ZERO,
            Metadata {
                type_name: "fact".into(),
                tenant: None,
                schema_version: 1,
                tags: vec![],
            },
            SecurityDescriptor {
                owner: "alice".into(),
                acl: vec![],
                classification: None,
            },
        );
        let schema = Schema::new("claim", 1);
        assert!(matches!(
            ko.validate_against(&schema, false),
            Err(KError::InvalidSchema(_))
        ));
    }

    #[test]
    fn validate_against_schema_rejects_version_mismatch() {
        let ko = KnowledgeObject::new(
            KOID::ZERO,
            Metadata {
                type_name: "fact".into(),
                tenant: None,
                schema_version: 1,
                tags: vec![],
            },
            SecurityDescriptor {
                owner: "alice".into(),
                acl: vec![],
                classification: None,
            },
        );
        let schema = Schema::new("fact", 2);
        assert!(matches!(
            ko.validate_against(&schema, false),
            Err(KError::InvalidSchema(_))
        ));
    }

    #[test]
    fn validate_against_schema_rejects_unknown_core_field() {
        let mut ko = KnowledgeObject::new(
            KOID::ZERO,
            Metadata {
                type_name: "fact".into(),
                tenant: None,
                schema_version: 1,
                tags: vec![],
            },
            SecurityDescriptor {
                owner: "alice".into(),
                acl: vec![],
                classification: None,
            },
        );
        ko.properties
            .insert("title".into(), Value::Text("hello".into()));
        ko.properties
            .insert("extra".into(), Value::Text("surprise".into()));
        let schema = Schema::new("fact", 1).require("title").allow("title");
        assert!(matches!(
            ko.validate_against(&schema, false),
            Err(KError::InvalidSchema(_))
        ));

        ko.properties.remove("extra");
        assert!(ko.validate_against(&schema, false).is_ok());
    }

    #[test]
    fn validate_open_world_schema_allows_unknown_core_fields() {
        let mut ko = KnowledgeObject::new(
            KOID::ZERO,
            Metadata {
                type_name: "fact".into(),
                tenant: None,
                schema_version: 1,
                tags: vec![],
            },
            SecurityDescriptor {
                owner: "alice".into(),
                acl: vec![],
                classification: None,
            },
        );
        ko.properties
            .insert("anything".into(), Value::Text("goes".into()));
        let schema = Schema::new("fact", 1);
        assert!(ko.validate_against(&schema, false).is_ok());
    }

    // -----------------------------------------------------------------------
    // MRFC-0060 Phase C1: Property type system
    // -----------------------------------------------------------------------

    #[test]
    fn value_type_name_returns_correct_names() {
        assert_eq!(Value::Null.type_name(), "Null");
        assert_eq!(Value::Bool(true).type_name(), "Bool");
        assert_eq!(Value::Int(42).type_name(), "Int");
        assert_eq!(Value::Float(std::f64::consts::PI).type_name(), "Float");
        assert_eq!(Value::Text("hi".into()).type_name(), "Text");
        assert_eq!(Value::Bytes(vec![1, 2, 3]).type_name(), "Bytes");
        assert_eq!(Value::List(vec![]).type_name(), "List");
        assert_eq!(Value::Map(BTreeMap::new()).type_name(), "Map");
    }

    fn prop(name: &str, vt: &str) -> SchemaProperty {
        SchemaProperty {
            name: name.into(),
            value_type: vt.into(),
            required: false,
            nullable: false,
            provenance_required: false,
            domain_constraints: Vec::new(),
        }
    }

    #[test]
    fn type_check_passes_for_matching_types() {
        assert!(Value::Bool(true).type_check(&prop("flag", "Bool")).is_ok());
        assert!(Value::Int(42).type_check(&prop("count", "Int")).is_ok());
        assert!(Value::Float(std::f64::consts::PI)
            .type_check(&prop("score", "Float"))
            .is_ok());
        assert!(Value::Text("hi".into())
            .type_check(&prop("name", "Text"))
            .is_ok());
        assert!(Value::Bytes(vec![1])
            .type_check(&prop("blob", "Bytes"))
            .is_ok());
        assert!(Value::List(vec![])
            .type_check(&prop("items", "List"))
            .is_ok());
        assert!(Value::Map(BTreeMap::new())
            .type_check(&prop("meta", "Map"))
            .is_ok());
    }

    #[test]
    fn type_check_rejects_type_mismatch() {
        let err = Value::Bool(true)
            .type_check(&prop("count", "Int"))
            .unwrap_err();
        assert!(err.contains("type mismatch"));
        assert!(err.contains("count"));
    }

    #[test]
    fn type_check_int_widens_to_float() {
        assert!(Value::Int(42).type_check(&prop("score", "Float")).is_ok());
    }

    #[test]
    fn type_check_text_accepted_as_datetime_and_json() {
        assert!(Value::Text("2024-01-01".into())
            .type_check(&prop("ts", "DateTime"))
            .is_ok());
        assert!(Value::Text("{\"a\":1}".into())
            .type_check(&prop("data", "Json"))
            .is_ok());
    }

    #[test]
    fn type_check_null_passes_when_nullable() {
        let p = SchemaProperty {
            name: "comment".into(),
            value_type: "Text".into(),
            required: false,
            nullable: true,
            provenance_required: false,
            domain_constraints: Vec::new(),
        };
        assert!(Value::Null.type_check(&p).is_ok());
    }

    #[test]
    fn type_check_null_fails_when_not_nullable() {
        let err = Value::Null.type_check(&prop("name", "Text")).unwrap_err();
        assert!(err.contains("not nullable"));
    }

    #[test]
    fn schema_builder_property_adds_typed_property() {
        let s = Schema::new("Person", 1)
            .property("name", "Text")
            .required_property("age", "Int")
            .nullable_property("nickname", "Text");
        assert_eq!(s.properties.len(), 3);
        assert!(s.find_property("age").unwrap().required);
        assert!(s.find_property("nickname").unwrap().nullable);
        assert!(!s.find_property("name").unwrap().required);
    }

    #[test]
    fn schema_find_property_returns_none_for_missing() {
        let s = Schema::new("Empty", 1);
        assert!(s.find_property("nope").is_none());
    }

    #[test]
    fn schema_unique_adds_constraint() {
        let s = Schema::new("User", 1)
            .unique(&["email"], UniquenessScope::Type)
            .unique(&["tenant_id", "username"], UniquenessScope::Tenant);
        assert_eq!(s.unique_constraints.len(), 2);
        assert_eq!(s.unique_constraints[0].properties, vec!["email"]);
        assert_eq!(s.unique_constraints[0].scope, UniquenessScope::Type);
        assert_eq!(
            s.unique_constraints[1].properties,
            vec!["tenant_id", "username"]
        );
        assert_eq!(s.unique_constraints[1].scope, UniquenessScope::Tenant);
    }

    // --- MRFC-0060 Phase C4: domain constraints ---

    #[test]
    fn domain_range_rejects_below_min() {
        let dc = DomainConstraint::Range {
            min: Some(0.0),
            max: None,
        };
        assert!(dc.validate(&Value::Int(-5)).is_err());
        assert!(dc.validate(&Value::Int(0)).is_ok());
    }

    #[test]
    fn domain_range_rejects_above_max() {
        let dc = DomainConstraint::Range {
            min: None,
            max: Some(100.0),
        };
        assert!(dc.validate(&Value::Float(101.0)).is_err());
        assert!(dc.validate(&Value::Float(99.9)).is_ok());
    }

    #[test]
    fn domain_length_enforces_min_max() {
        let dc = DomainConstraint::Length {
            min: Some(3),
            max: Some(10),
        };
        assert!(dc.validate(&Value::Text("ab".into())).is_err());
        assert!(dc.validate(&Value::Text("abc".into())).is_ok());
        assert!(dc.validate(&Value::Text("abcdefghij".into())).is_ok());
        assert!(dc.validate(&Value::Text("abcdefghijk".into())).is_err());
    }

    #[test]
    fn domain_enum_rejects_unknown_value() {
        let dc = DomainConstraint::Enum(vec![Value::Text("a".into()), Value::Text("b".into())]);
        assert!(dc.validate(&Value::Text("a".into())).is_ok());
        assert!(dc.validate(&Value::Text("c".into())).is_err());
    }

    #[test]
    fn domain_pattern_regex_matching() {
        let dc = DomainConstraint::Pattern(r"\.txt$".into());
        assert!(dc.validate(&Value::Text("file.txt".into())).is_ok());
        assert!(dc.validate(&Value::Text("file.md".into())).is_err());
    }

    #[test]
    fn domain_format_email() {
        let dc = DomainConstraint::Format("email".into());
        assert!(dc.validate(&Value::Text("a@b.com".into())).is_ok());
        assert!(dc.validate(&Value::Text("not-an-email".into())).is_err());
    }

    // --- MRFC-0060 Phase C4: check expression evaluation ---

    #[test]
    fn check_expr_end_date_ge_start_date() {
        use CheckExpression::*;
        // end_date >= start_date
        let expr = Compare {
            op: CompareOp::Gte,
            left: Box::new(Property("end_date".into())),
            right: Box::new(Property("start_date".into())),
        };
        let mut props = PropertyMap::new();
        props.insert("start_date".into(), Value::Text("2024-01-01".into()));
        props.insert("end_date".into(), Value::Text("2024-12-31".into()));
        assert!(expr.evaluate(&props).unwrap());
        // Flipped — should fail
        props.insert("end_date".into(), Value::Text("2023-01-01".into()));
        assert!(!expr.evaluate(&props).unwrap());
    }

    #[test]
    fn check_expr_and_or_not() {
        use CheckExpression::*;
        // (age >= 18) AND (age <= 120)
        let expr = And(
            Box::new(Compare {
                op: CompareOp::Gte,
                left: Box::new(Property("age".into())),
                right: Box::new(Literal(Value::Int(18))),
            }),
            Box::new(Compare {
                op: CompareOp::Lte,
                left: Box::new(Property("age".into())),
                right: Box::new(Literal(Value::Int(120))),
            }),
        );
        let mut props = PropertyMap::new();
        props.insert("age".into(), Value::Int(25));
        assert!(expr.evaluate(&props).unwrap());
        props.insert("age".into(), Value::Int(150));
        assert!(!expr.evaluate(&props).unwrap());
    }

    #[test]
    fn check_expr_not() {
        use CheckExpression::*;
        // NOT (status = "deleted")
        let expr = Not(Box::new(Compare {
            op: CompareOp::Eq,
            left: Box::new(Property("status".into())),
            right: Box::new(Literal(Value::Text("deleted".into()))),
        }));
        let mut props = PropertyMap::new();
        props.insert("status".into(), Value::Text("active".into()));
        assert!(expr.evaluate(&props).unwrap());
        props.insert("status".into(), Value::Text("deleted".into()));
        assert!(!expr.evaluate(&props).unwrap());
    }

    #[test]
    fn schema_domain_constraint_builder() {
        let s = Schema::new("Product", 1)
            .property("price", "Float")
            .domain_constraint(DomainConstraint::Range {
                min: Some(0.0),
                max: Some(9999.99),
            })
            .check(
                "price_positive",
                CheckExpression::Compare {
                    op: CompareOp::Gte,
                    left: Box::new(CheckExpression::Property("price".into())),
                    right: Box::new(CheckExpression::Literal(Value::Float(0.0))),
                },
            );
        assert_eq!(s.properties[0].domain_constraints.len(), 1);
        assert_eq!(s.check_constraints.len(), 1);
        assert_eq!(s.check_constraints[0].name, "price_positive");
    }

    // --- MRFC-0060 Phase C4: expression parser ---

    #[test]
    fn parse_simple_comparison() {
        let expr = CheckExpression::parse("end_date >= start_date").unwrap();
        let mut props = PropertyMap::new();
        props.insert("start_date".into(), Value::Text("2024-01-01".into()));
        props.insert("end_date".into(), Value::Text("2024-12-31".into()));
        assert!(expr.evaluate(&props).unwrap());
    }

    #[test]
    fn parse_and_or_precedence() {
        // "age >= 18 AND age <= 120" — AND binds tighter than OR
        let expr = CheckExpression::parse("age >= 18 AND age <= 120").unwrap();
        let mut props = PropertyMap::new();
        props.insert("age".into(), Value::Int(25));
        assert!(expr.evaluate(&props).unwrap());
        props.insert("age".into(), Value::Int(5));
        assert!(!expr.evaluate(&props).unwrap());
    }

    #[test]
    fn parse_not_expression() {
        let expr = CheckExpression::parse("NOT (status == \"deleted\")").unwrap();
        let mut props = PropertyMap::new();
        props.insert("status".into(), Value::Text("active".into()));
        assert!(expr.evaluate(&props).unwrap());
        props.insert("status".into(), Value::Text("deleted".into()));
        assert!(!expr.evaluate(&props).unwrap());
    }

    #[test]
    fn parse_or_expression() {
        let expr = CheckExpression::parse("role == \"admin\" OR role == \"superadmin\"").unwrap();
        let mut props = PropertyMap::new();
        props.insert("role".into(), Value::Text("admin".into()));
        assert!(expr.evaluate(&props).unwrap());
        props.insert("role".into(), Value::Text("user".into()));
        assert!(!expr.evaluate(&props).unwrap());
    }

    #[test]
    fn parse_at_prefix_property() {
        let expr = CheckExpression::parse("@price >= 0").unwrap();
        let mut props = PropertyMap::new();
        props.insert("price".into(), Value::Float(9.99));
        assert!(expr.evaluate(&props).unwrap());
    }

    #[test]
    fn parse_literals_true_false_null() {
        let expr = CheckExpression::parse("active == true").unwrap();
        let mut props = PropertyMap::new();
        props.insert("active".into(), Value::Bool(true));
        assert!(expr.evaluate(&props).unwrap());
        props.insert("active".into(), Value::Bool(false));
        assert!(!expr.evaluate(&props).unwrap());
    }

    // --- Enhanced format validation tests ---

    #[test]
    fn domain_format_email_rejects_invalid() {
        let dc = DomainConstraint::Format("email".into());
        assert!(dc.validate(&Value::Text("user@example.com".into())).is_ok());
        assert!(dc.validate(&Value::Text("not-an-email".into())).is_err());
        assert!(dc
            .validate(&Value::Text("@missing-local.com".into()))
            .is_err());
    }

    #[test]
    fn domain_format_url_rejects_invalid() {
        let dc = DomainConstraint::Format("url".into());
        assert!(dc
            .validate(&Value::Text("https://example.com/path".into()))
            .is_ok());
        assert!(dc
            .validate(&Value::Text("http://localhost:8080".into()))
            .is_ok());
        assert!(dc.validate(&Value::Text("not-a-url".into())).is_err());
    }

    #[test]
    fn domain_format_uuid_rejects_invalid() {
        let dc = DomainConstraint::Format("uuid".into());
        assert!(dc
            .validate(&Value::Text("550e8400-e29b-41d4-a716-446655440000".into()))
            .is_ok());
        assert!(dc.validate(&Value::Text("not-a-uuid".into())).is_err());
    }

    #[test]
    fn domain_format_date_rejects_bad_month() {
        let dc = DomainConstraint::Format("date".into());
        assert!(dc.validate(&Value::Text("2024-06-15".into())).is_ok());
        assert!(dc.validate(&Value::Text("2024-13-01".into())).is_err()); // month 13
    }

    #[test]
    fn domain_format_datetime_rejects_malformed() {
        let dc = DomainConstraint::Format("datetime".into());
        assert!(dc
            .validate(&Value::Text("2024-06-15T14:30:00Z".into()))
            .is_ok());
        assert!(dc
            .validate(&Value::Text("2024-06-15T14:30:00+05:30".into()))
            .is_ok());
        assert!(dc.validate(&Value::Text("not-a-datetime".into())).is_err());
    }

    // --- C9 programmable constraint tests ---

    #[test]
    fn c9_arithmetic_in_comparison() {
        // @total == @price * @quantity
        let expr = CheckExpression::Compare {
            op: CompareOp::Eq,
            left: Box::new(CheckExpression::Property("total".into())),
            right: Box::new(CheckExpression::Arith(
                Box::new(CheckExpression::Property("price".into())),
                ArithOp::Mul,
                Box::new(CheckExpression::Property("quantity".into())),
            )),
        };
        let mut props = PropertyMap::new();
        props.insert("total".into(), Value::Int(100));
        props.insert("price".into(), Value::Int(20));
        props.insert("quantity".into(), Value::Int(5));
        assert!(expr.evaluate(&props).unwrap());

        props.insert("total".into(), Value::Int(99));
        assert!(!expr.evaluate(&props).unwrap());
    }

    #[test]
    fn c9_arithmetic_add_and_compare() {
        // @a + @b > 10
        let expr = CheckExpression::Compare {
            op: CompareOp::Gt,
            left: Box::new(CheckExpression::Arith(
                Box::new(CheckExpression::Property("a".into())),
                ArithOp::Add,
                Box::new(CheckExpression::Property("b".into())),
            )),
            right: Box::new(CheckExpression::Literal(Value::Int(10))),
        };
        let mut props = PropertyMap::new();
        props.insert("a".into(), Value::Int(7));
        props.insert("b".into(), Value::Int(5));
        assert!(expr.evaluate(&props).unwrap());

        props.insert("a".into(), Value::Int(3));
        assert!(!expr.evaluate(&props).unwrap());
    }

    #[test]
    fn c9_conditional_if() {
        // IF @status == "active" THEN @email != NULL ELSE true
        use CheckExpression::*;
        let expr = If(
            Box::new(Compare {
                op: CompareOp::Eq,
                left: Box::new(Property("status".into())),
                right: Box::new(Literal(Value::Text("active".into()))),
            }),
            Box::new(Compare {
                op: CompareOp::Neq,
                left: Box::new(Property("email".into())),
                right: Box::new(Literal(Value::Null)),
            }),
            Box::new(Literal(Value::Bool(true))),
        );

        // Active with email → passes
        let mut props = PropertyMap::new();
        props.insert("status".into(), Value::Text("active".into()));
        props.insert("email".into(), Value::Text("u@x.com".into()));
        assert!(expr.evaluate(&props).unwrap());

        // Active with null email → fails
        props.insert("email".into(), Value::Null);
        assert!(!expr.evaluate(&props).unwrap());

        // Inactive with null email → passes (else branch)
        props.insert("status".into(), Value::Text("inactive".into()));
        assert!(expr.evaluate(&props).unwrap());
    }

    #[test]
    fn c9_div_by_zero_error() {
        // @a / 0
        let expr = CheckExpression::Arith(
            Box::new(CheckExpression::Property("a".into())),
            ArithOp::Div,
            Box::new(CheckExpression::Literal(Value::Int(0))),
        );
        let mut props = PropertyMap::new();
        props.insert("a".into(), Value::Int(42));
        let result = expr.evaluate(&props);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("division by zero"));
    }

    #[test]
    fn c9_int_float_widening() {
        // @int_val + @float_val → Float
        let expr = CheckExpression::Compare {
            op: CompareOp::Eq,
            left: Box::new(CheckExpression::Arith(
                Box::new(CheckExpression::Property("int_val".into())),
                ArithOp::Add,
                Box::new(CheckExpression::Property("float_val".into())),
            )),
            right: Box::new(CheckExpression::Literal(Value::Float(7.5))),
        };
        let mut props = PropertyMap::new();
        props.insert("int_val".into(), Value::Int(5));
        props.insert("float_val".into(), Value::Float(2.5));
        assert!(expr.evaluate(&props).unwrap());
    }

    #[test]
    fn c9_text_concatenation() {
        // @first + @last == "JohnDoe"
        let expr = CheckExpression::Compare {
            op: CompareOp::Eq,
            left: Box::new(CheckExpression::Arith(
                Box::new(CheckExpression::Property("first".into())),
                ArithOp::Add,
                Box::new(CheckExpression::Property("last".into())),
            )),
            right: Box::new(CheckExpression::Literal(Value::Text("JohnDoe".into()))),
        };
        let mut props = PropertyMap::new();
        props.insert("first".into(), Value::Text("John".into()));
        props.insert("last".into(), Value::Text("Doe".into()));
        assert!(expr.evaluate(&props).unwrap());
    }

    #[test]
    fn c9_collect_refs_includes_arith_and_if() {
        use CheckExpression::*;
        let expr = If(
            Box::new(Property("status".into())),
            Box::new(Arith(
                Box::new(Property("a".into())),
                ArithOp::Add,
                Box::new(Property("b".into())),
            )),
            Box::new(Property("fallback".into())),
        );
        let mut refs = Vec::new();
        expr.collect_refs(&mut refs);
        refs.sort();
        // status, a, b, fallback — all collected recursively
        assert_eq!(refs, vec!["a", "b", "fallback", "status"]);
    }
}
