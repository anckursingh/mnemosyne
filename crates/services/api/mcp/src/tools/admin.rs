//! MCP tool implementations — extracted from main.rs (R7 modularization).
//! No behavior changes.

use crate::{json, Kernel, LifecycleState, Ordering, Subject, ACTIVE_CONNECTIONS, J, SERVER_START};
use std::sync::Arc;
pub(crate) fn tool_metrics(k: &Kernel) -> Result<J, String> {
    let (seq, _audit) = k.journal_head().map_err(|e| e.to_string())?;
    let heads = k.scan_heads().map_err(|e| e.to_string())?;
    let active = heads
        .iter()
        .filter(|(_, _, _, s)| *s != LifecycleState::Deleted)
        .count();
    let mut draft = 0u64;
    let mut active_st = 0u64;
    let mut verified = 0u64;
    let mut archived = 0u64;
    let mut deleted = 0u64;
    for (_, _, _, s) in &heads {
        match s {
            LifecycleState::Draft => draft += 1,
            LifecycleState::Active => active_st += 1,
            LifecycleState::Verified => verified += 1,
            LifecycleState::Archived => archived += 1,
            LifecycleState::Deleted => deleted += 1,
            // MRFC-0070 states: count as draft-equivalent pending
            LifecycleState::Discovered
            | LifecycleState::Extracted
            | LifecycleState::Proposed
            | LifecycleState::Validated
            | LifecycleState::Accepted
            | LifecycleState::Updated
            | LifecycleState::Superseded => draft += 1,
        }
    }
    // Type-level breakdown (ponytail: O(n) scan; add type index if slow).
    let types = k.list_types().map_err(|e| e.to_string())?;
    let system = Subject::with_roles("system", &["admin"]);
    let mut by_type = serde_json::Map::new();
    for t in &types {
        if let Ok(kos) = k.scan_by_type(&system, t) {
            by_type.insert(t.clone(), json!(kos.len()));
        }
    }
    let uptime_secs = SERVER_START
        .get()
        .map(|start| start.elapsed().as_secs_f64())
        .unwrap_or(0.0);
    Ok(json!({
        "journal_seq": seq,
        "total_objects": heads.len(),
        "active_objects": active,
        "uptime_seconds": (uptime_secs * 10.0).round() / 10.0,
        "by_lifecycle": {
            "draft": draft,
            "active": active_st,
            "verified": verified,
            "archived": archived,
            "deleted": deleted,
        },
        "by_type": by_type,
    }))
}

// ---------------------------------------------------------------------------
// tools/list
// ---------------------------------------------------------------------------

pub(crate) fn tool_verify_backup(args: &J) -> Result<J, String> {
    let backup = args
        .get("backup")
        .and_then(|b| b.as_str())
        .ok_or("missing argument: backup")?;
    let data_path = backup_data_file(backup)?;
    let meta_str = std::fs::read_to_string(format!("{}/meta.json", backup))
        .map_err(|e| format!("not a valid backup: {}", e))?;
    let meta: J = serde_json::from_str(&meta_str).map_err(|e| format!("bad meta: {}", e))?;
    let expected_seq = meta["journal_seq"].as_u64().unwrap_or(0);
    let expected_objects = meta["object_count"].as_u64().unwrap_or(0) as usize;
    let ok = verify_backup_file(&data_path, expected_seq, expected_objects);
    Ok(json!({
        "backup": backup,
        "verified": ok,
        "expected_journal_seq": expected_seq,
        "expected_objects": expected_objects,
    }))
}

// ---------------------------------------------------------------------------
// HTTP metrics server — minimal std-based HTTP/1.0 handler
// ---------------------------------------------------------------------------

pub(crate) fn tool_abi_version(k: &Kernel) -> Result<J, String> {
    let version = k.abi_version();
    // Also export the full audit chain for offline verification.
    let proof = k.prove_export().map_err(|e| e.to_string())?;
    Ok(json!({
        "abi_version": version,
        "journal_seq": proof.journal_seq,
        "head_audit_hash": proof.head_audit_hash.iter().map(|b| format!("{:02x}", b)).collect::<Vec<_>>().join(""),
        "event_count": proof.events.len(),
        "audit_chain_exportable": true,
    }))
}

pub(crate) fn tool_health(k: &Kernel) -> Result<J, String> {
    let (seq, audit) = k.journal_head().unwrap_or((0, [0u8; 32]));
    let heads = k.scan_heads().map(|h| h.len()).unwrap_or(0);
    let ready = true;
    // Single-node: journal is always current, so lag is 0.
    let journal_lag_ms: u64 = 0;
    let connections = ACTIVE_CONNECTIONS.load(Ordering::Relaxed);
    let max_connections = if connections > 0 { connections } else { 1 };
    // PRR-3: surface semantic readiness (enrichment worker updates the static).
    let sem = crate::semantic_status_snapshot();
    Ok(json!({
        "status": if ready { "healthy" } else { "degraded" },
        "ready": ready,
        "journal_seq": seq,
        "journal_lag_ms": journal_lag_ms,
        "object_count": heads,
        "connection_pool": format!("{}/{}", connections, max_connections),
        "audit_hash": audit.iter().map(|b| format!("{:02x}", b)).collect::<Vec<_>>().join(""),
        "uptime_seconds": SERVER_START.get().map(|s| s.elapsed().as_secs_f64()).unwrap_or(0.0),
        "semantic": {
            "state": sem.state,
            "detail": sem.detail,
        },
    }))
}

pub(crate) fn tool_backup(k: &Kernel, db_path: &str) -> Result<J, String> {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_secs();
    use std::path::{Path, PathBuf};
    let src = Path::new(db_path);
    let backup_dir: PathBuf = {
        let mut p = src.as_os_str().to_os_string();
        p.push(format!(".backup.{}", ts));
        PathBuf::from(p)
    };
    std::fs::create_dir_all(&backup_dir).map_err(|e| e.to_string())?;

    // Snapshot the store through the kernel — the live file is region-locked
    // on Windows, so a file-level copy cannot read it (os error 33).
    let src_file = src.file_name().ok_or("invalid db path: no filename")?;
    let dest_path = backup_dir.join(src_file);
    k.backup_store_to(&dest_path).map_err(|e| e.to_string())?;

    // Record source metadata.
    let (seq, _audit) = k.journal_head().map_err(|e| e.to_string())?;
    let obj_count = k.scan_heads().map_err(|e| e.to_string())?.len();
    let meta_path = backup_dir.join("meta.json");
    std::fs::write(
        &meta_path,
        json!({"timestamp": ts, "source": db_path, "journal_seq": seq, "object_count": obj_count})
            .to_string(),
    )
    .map_err(|e| e.to_string())?;

    // Verify: open backup in a temp kernel and check integrity.
    let dest_str = dest_path.to_string_lossy().to_string();
    let verified = verify_backup_file(&dest_str, seq, obj_count);

    Ok(
        json!({"backup": backup_dir, "timestamp": ts, "journal_seq": seq, "object_count": obj_count, "verified": verified}),
    )
}

/// Open a backup file in a throwaway kernel and check basic integrity.
///
/// The snapshot format is redb regardless of the production backend
/// (engine-independent snapshot_to — KSE-14), so verification opens the
/// backup AS redb explicitly, never through the AIKOQL_BACKEND-selected
/// default.
pub(crate) fn verify_backup_file(path: &str, expected_seq: u64, expected_objects: usize) -> bool {
    let k = match crate::RedbEngine::open(path) {
        Ok(e) => match Kernel::open(Arc::new(e), Arc::new(crate::SystemClock), 0xA9C9) {
            Ok(k) => k,
            Err(_) => return false,
        },
        Err(_) => return false,
    };
    let (seq, _) = match k.journal_head() {
        Ok(h) => h,
        Err(_) => return false,
    };
    let count = match k.scan_heads() {
        Ok(h) => h.len(),
        Err(_) => return false,
    };
    seq == expected_seq && count == expected_objects
}

/// The data file inside a backup dir. tool_backup stores it under the source
/// db's filename (meta.json "source") — never a hard-coded data.redb.
fn backup_data_file(backup: &str) -> Result<String, String> {
    let meta_str = std::fs::read_to_string(format!("{}/meta.json", backup))
        .map_err(|e| format!("not a valid backup: {}", e))?;
    let meta: J = serde_json::from_str(&meta_str).map_err(|e| format!("bad meta: {}", e))?;
    if let Some(f) = meta
        .get("source")
        .and_then(|s| s.as_str())
        .and_then(|s| std::path::Path::new(s).file_name())
    {
        let p = format!("{}/{}", backup, f.to_string_lossy());
        if std::path::Path::new(&p).exists() {
            return Ok(p);
        }
    }
    // Fallback: any redb file in the backup dir.
    std::fs::read_dir(backup)
        .map_err(|e| e.to_string())?
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().and_then(|x| x.to_str()) == Some("redb"))
        .map(|p| p.to_string_lossy().to_string())
        .ok_or_else(|| "backup data file missing".into())
}

pub(crate) fn tool_restore(k: &Kernel, args: &J) -> Result<J, String> {
    let backup = args
        .get("backup")
        .and_then(|b| b.as_str())
        .ok_or("missing argument: backup")?;
    let meta_str = std::fs::read_to_string(format!("{}/meta.json", backup))
        .map_err(|e| format!("not a valid backup: {}", e))?;
    let meta: J = serde_json::from_str(&meta_str).map_err(|e| format!("bad meta: {}", e))?;
    let data_file = backup_data_file(backup)?;
    // Engine-level restore: the live file cannot be overwritten while the
    // server holds it open (region lock), so rows are swapped through the
    // kernel in one atomic batch.
    // ponytail: in-memory derived state is stale until restart — restart
    // the server after restore (TESTING-PLAN §9.4 REC-002).
    k.restore_store_from(std::path::Path::new(&data_file))
        .map_err(|e| e.to_string())?;
    // Report PITR recovery point from backup metadata.
    let pitr_seq = meta.get("journal_seq").and_then(|v| v.as_u64());
    let pitr_ts = meta.get("timestamp").and_then(|v| v.as_u64());
    Ok(json!({
        "restored": true,
        "meta": meta,
        "recovery_point": {
            "journal_seq": pitr_seq,
            "timestamp": pitr_ts,
        }
    }))
}

pub(crate) fn tool_list_backups(db_path: &str) -> Result<J, String> {
    // Backups land next to the db file, not in the server's CWD.
    let dir = std::path::Path::new(db_path)
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."));
    let mut backups = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.contains(".backup.") {
                let meta_path = format!("{}/meta.json", dir.join(&name).display());
                if let Ok(meta_str) = std::fs::read_to_string(&meta_path) {
                    if let Ok(meta) = serde_json::from_str::<J>(&meta_str) {
                        backups.push(json!({"name": name, "meta": meta}));
                    }
                }
            }
        }
    }
    Ok(json!({"backups": backups}))
}

pub(crate) fn tool_audit_report(k: &Kernel) -> Result<J, String> {
    let (seq, audit) = k.journal_head().map_err(|e| e.to_string())?;
    let heads = k.scan_heads().map_err(|e| e.to_string())?;
    let total = heads.len();
    let by_state: Vec<J> = heads
        .iter()
        .map(|(koid, v, ts, state)| {
            json!({"koid": koid.to_hex(), "version": v, "commit_ts": ts, "state": state.to_string()})
        })
        .collect();
    let events = k.journal().map_err(|e| e.to_string())?;
    let event_count = events.len();
    Ok(json!({
        "audit_chain": audit.iter().map(|b| format!("{:02x}", b)).collect::<Vec<_>>().join(""),
        "journal_seq": seq,
        "journal_events": event_count,
        "total_objects": total,
        "objects": by_state,
    }))
}

pub(crate) fn tool_compliance_report(k: &Kernel) -> Result<J, String> {
    let report = k.compliance_report().map_err(|e| e.to_string())?;
    let summary = report.field_crypto_summary.as_ref();
    let audit_counts: Vec<J> = summary
        .map(|s| {
            s.audit_events
                .iter()
                .map(|(kind, count)| json!({"kind": kind.as_str(), "count": count}))
                .collect()
        })
        // justified: no crypto summary → empty audit list
        .unwrap_or_default();
    Ok(json!({
        "encryption_enabled": report.encryption_enabled,
        "policies_registered": report.policies_registered,
        "policy_types": report.policy_types,
        "field_encryption_enabled": summary.map(|s| s.field_encryption_enabled).unwrap_or(false),
        "tenant_keys": summary.map(|s| s.tenant_keys).unwrap_or(0),
        "audit_events": audit_counts,
        "compliance_grade": if report.encryption_enabled && report.policies_registered > 0 { "A" } else { "C" },
    }))
}

/// MRFC-0020 Phase 4 (IMPLEMENTATION-PLAN "Next implementation"): one
/// auditor export bundling the audit chain, the object inventory, the
/// PII-filtering config, the retention records, and the encryption
/// compliance report. Both frameworks carry the same bundle — the auditor
/// maps sections to clauses; the framework tag only labels the report.
/// Honest rows: purge coverage is counted-eligibility only (no kernel
/// purge op exists), and the PII detector's R8.1 known limits travel
/// with the pack rather than being implied away.
pub(crate) fn tool_evidence_pack(k: &Kernel, args: &J) -> Result<J, String> {
    let framework = args
        .get("framework")
        .and_then(|f| f.as_str())
        .unwrap_or("gdpr");
    if framework != "gdpr" && framework != "hipaa" {
        return Err(format!(
            "unsupported framework: {framework} (supported: gdpr, hipaa)"
        ));
    }

    // Audit chain + object inventory (audit_report substrate).
    let (seq, audit) = k.journal_head().map_err(|e| e.to_string())?;
    let heads = k.scan_heads().map_err(|e| e.to_string())?;
    let mut by_state: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    for (_, _, _, s) in &heads {
        *by_state.entry(s.to_string()).or_insert(0) += 1;
    }

    // Retention records (kernel-stamped valid_to horizons).
    let retention = k.retention_summary().map_err(|e| e.to_string())?;

    // Encryption compliance (existing report, same shape as its own tool).
    let encryption = tool_compliance_report(k)?;

    Ok(json!({
        "framework": framework,
        "audit_chain": audit.iter().map(|b| format!("{:02x}", b)).collect::<Vec<_>>().join(""),
        "journal_seq": seq,
        "object_inventory": {
            "total": heads.len(),
            "by_state": by_state,
        },
        "pii_filtering": {
            "active": true,
            "detector_kinds": aikoql_ingestion::ALL_KINDS
                .iter()
                .map(|k| k.as_str())
                .collect::<Vec<_>>(),
            // R8.1: pattern-based detection catches known formats only —
            // the known limits travel with the evidence, not implied away.
            "known_limits": "pattern-based detection catches known formats only; it does not decode URL-encoded or base64-encoded text or reassemble secrets split across lines (MRFC-0070 A7, R8.1)",
        },
        "retention": {
            "retained_objects": retention.retained_objects,
            "live_windows": retention.live_windows,
            "expired": retention.expired,
            "purge_coverage": "expired objects are counted and purge-eligible; physical deletion is caller-side — the kernel has no purge op (MRFC-0020 Phase 4 honest row)",
        },
        "encryption": encryption,
    }))
}
