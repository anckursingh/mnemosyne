//! Wave 3.1 (MVP-QA-003A) — W31-SCALE-001 knowledge complexity scaling.
//!
//! 1K / 10K / 100K synthetic knowledge units — n entities + n facts +
//! 0.8n relations (the retrieval-index records; "KOs" in the spec's
//! sense, since the IR is the pre-KO candidate surface — declared in the
//! report). PR CI runs 1K + 10K (PR#2 review SE-06);
//! `W31_SCALE_NIGHTLY=1` adds the 100K row. The 1M row is the R14
//! pointer: benchmarks/benches/scale.rs (criterion, AIKOQL_BENCH_SCALE
//! knob, ~4GB at 1M) — asserted to exist and to carry the knob, so the
//! pointer cannot rot silently.
//!
//! Per scale: 12 probes × 3 reps —
//! - 8 tier lookups ("What is the tier of aaaa?"),
//! - 3 depends_on→sla relation probes (the sla fact of the service the
//!   customer depends on — each sla text embeds its service index, so
//!   the judge verifies the RIGHT service packed, not just any),
//! - 1 absent-entity trap ("What is the tier of zzzz?") expecting a
//!   healthy-empty pack (the UNK-001 refusal boundary).
//!
//! Measured (spec): task success, p50/p95 latency, tokens, G11 cost,
//! context size (tokens/probe), retrieval work (index sizes). The spec's
//! question — does the product advantage survive increasing knowledge
//! complexity — is answered by the printed table; success 12/12 is
//! asserted (the survival IS the question, and a lookup/relation/trap
//! battery is the deterministic surface).
//!
//! Why letter names: entity names are pure base-26 tokens ("aaaa"..).
//! A first pass with ID-style names (Customer0..CustomerN) flooded the
//! pack — every name shares the ≥4-char "customer" prefix, so the
//! partial-prefix credit ranked all of them for every probe and the
//! ambiguity retraction rendered the whole group (measured, kept in
//! losses.md). Letter names share no prefix, so only exact matches rank
//! and the pack stays scoped — the isolated-scale shape the spec asks
//! for. The IR is built directly, not extracted: extraction is not what
//! SCALE-001 measures (the retrieval leg is). Every statement embeds its
//! distinguishing entity token — merge dedups facts by statement
//! equality, and identical statements would collapse.

mod common;

use std::time::Instant;

use aikoql_ingestion::{
    compile_context, render_context_markdown, EntityCandidate, FactCandidate, KnowledgeIr,
    RelationCandidate,
};
use common::wave31_sim::{cost, payload_has};

const BUDGET: usize = 300;
const REPS: usize = 3;
const NIGHTLY_ENV: &str = "W31_SCALE_NIGHTLY";

/// PR#2 review SE-06: the PR gate runs 1K + 10K; `W31_SCALE_NIGHTLY=1`
/// adds the 100K row for the canonical scaling report. Strict opt-in —
/// any other value fails.
fn sizes() -> &'static [usize] {
    match std::env::var(NIGHTLY_ENV) {
        Err(std::env::VarError::NotPresent) => &[1_000, 10_000],
        Ok(v) if v == "1" => &[1_000, 10_000, 100_000],
        other => panic!("{NIGHTLY_ENV} strict opt-in: unset or 1, got {other:?}"),
    }
}

/// The synthetic candidates' provenance (ingestion-level Evidence — the
/// kernel Evidence type is the KO surface, not the IR surface).
fn evd() -> aikoql_ingestion::Evidence {
    aikoql_ingestion::Evidence {
        document_id: Some("scale-synth".into()),
        page: None,
        source: None,
        extractor: "scale-synth".into(),
        model: None,
        ..Default::default()
    }
}

/// Base-26 name for index i: "aaaa", "aaab", … (one token, no shared
/// ≥4-char prefix between distinct names — the partial-prefix credit
/// stays at zero across the whole synthetic world).
fn name4(mut i: usize) -> String {
    let mut s = [b'a'; 4];
    for p in (0..4).rev() {
        s[p] += (i % 26) as u8;
        i /= 26;
    }
    String::from_utf8(s.to_vec()).expect("ascii")
}

/// What a probe expects: a non-empty pack carrying every unit token, or
/// the healthy-empty refusal (trap).
enum Expect {
    Has(Vec<String>),
    Empty,
}

fn tier(k: usize) -> &'static str {
    ["gold", "silver", "bronze"][k % 3]
}

/// The synthetic knowledge unit set: customers (80%) and services (20%).
fn synth_ir(n: usize) -> KnowledgeIr {
    let services = (n / 5).max(1);
    let customers = n - services;
    let evd = evd();

    let mut entities = Vec::with_capacity(n);
    let mut facts = Vec::with_capacity(n);
    for k in 0..customers {
        let name = name4(k);
        entities.push(EntityCandidate {
            name: name.clone(),
            type_hint: Some("Customer".into()),
            mentions: vec![name.clone()],
            confidence: 0.9,
            evidence: evd.clone(),
        });
        facts.push(FactCandidate {
            statement: format!("{name} tier is {}", tier(k)),
            entities: vec![name],
            confidence: 0.9,
            evidence: evd.clone(),
            snippet: None,
        });
    }
    for m in 0..services {
        let name = name4(customers + m);
        entities.push(EntityCandidate {
            name: name.clone(),
            type_hint: Some("Service".into()),
            mentions: vec![name.clone()],
            confidence: 0.9,
            evidence: evd.clone(),
        });
        // The sla text embeds its service index — the judge verifies the
        // right service, not just any sla fact.
        facts.push(FactCandidate {
            statement: format!("{name} sla is platinums{m}"),
            entities: vec![name],
            confidence: 0.9,
            evidence: evd.clone(),
            snippet: None,
        });
    }
    let mut relations = Vec::with_capacity(customers);
    for k in 0..customers {
        relations.push(RelationCandidate {
            subject: name4(k),
            predicate: "depends_on".into(),
            object: name4(customers + k % services),
            confidence: 0.9,
            evidence: evd.clone(),
        });
    }

    KnowledgeIr {
        entities,
        relations,
        facts,
        events: vec![],
        temporal: vec![],
        document_id: Some("scale-synth".into()),
        source_revision: None,
        content_trust: None,
        page_count: 1,
        extractor: "scale-synth".into(),
    }
}

fn battery(customers: usize, services: usize) -> Vec<(String, Expect)> {
    let ks = [
        0usize,
        1,
        customers / 10,
        customers / 4,
        customers / 2,
        3 * customers / 4,
        customers - 2,
        customers - 1,
    ];
    let mut out: Vec<(String, Expect)> = ks
        .iter()
        .map(|&k| {
            (
                format!("What is the tier of {}?", name4(k)),
                Expect::Has(vec![tier(k).to_string()]),
            )
        })
        .collect();
    for &k in &[0usize, customers / 3, customers - 1] {
        let m = k % services;
        out.push((
            format!("What is the sla of the service {} depends on?", name4(k)),
            Expect::Has(vec![format!("platinums{m}")]),
        ));
    }
    out.push(("What is the tier of zzzz?".to_string(), Expect::Empty));
    out
}

/// p50/p95 of sorted micros (nearest-rank — the COMP-001 convention).
fn pct(micros: &[u128], p: f64) -> u128 {
    if micros.is_empty() {
        return 0;
    }
    micros[((micros.len() - 1) as f64 * p).round() as usize]
}

#[test]
fn w31_scale_001_knowledge_complexity_scaling() {
    for &n in sizes() {
        let ir = synth_ir(n);
        let services = (n / 5).max(1);
        let customers = n - services;
        let probes = battery(customers, services);

        let (mut success, mut tokens, mut answered) = (0usize, 0usize, 0usize);
        let mut micros = Vec::with_capacity(probes.len() * REPS);
        for (pi, (text, expect)) in probes.iter().enumerate() {
            for _ in 0..REPS {
                let t0 = Instant::now();
                let pkg = compile_context(text.as_str(), &ir, BUDGET);
                let payload = render_context_markdown(&pkg);
                micros.push(t0.elapsed().as_micros());
                let ok = match expect {
                    Expect::Has(units) => {
                        !payload.is_empty() && units.iter().all(|u| payload_has(&payload, u))
                    }
                    Expect::Empty => payload.trim().is_empty(),
                };
                eprintln!(
                    "[W31-SCALE-001 n={n} P{pi}] {} — {} tokens",
                    if ok { "ok  " } else { "FAIL" },
                    payload.len() / 4,
                );
                success += ok as usize;
                tokens += payload.len() / 4;
                if !payload.trim().is_empty() {
                    answered += 1;
                }
            }
        }
        micros.sort();

        let work = (
            ir.entities.len(),
            ir.facts.len(),
            ir.relations.len(),
            ir.entities.len() + ir.facts.len() + ir.relations.len(),
        );
        eprintln!(
            "[W31-SCALE-001 n={n}] retrieval work (entities/facts/relations/total): \
             {}/{}/{}/{}",
            work.0, work.1, work.2, work.3
        );
        eprintln!(
            "[W31-SCALE-001 n={n}] task success {}/{} — p50 {}us p95 {}us — \
             context {} tokens over {} probes ({:.0}/probe) — G11 cost ${:.5} — \
             ({} answered, {} empty-pack refusals)",
            success,
            probes.len() * REPS,
            pct(&micros, 0.50),
            pct(&micros, 0.95),
            tokens,
            probes.len() * REPS,
            tokens as f32 / (probes.len() * REPS) as f32,
            cost(tokens, answered),
            answered,
            probes.len() * REPS - answered,
        );
        assert_eq!(
            success,
            probes.len() * REPS,
            "SCALE-001 n={n}: {} probe-reps failed — the advantage did not survive",
            probes.len() * REPS - success
        );
    }

    // ── the 1M row: the R14 pointer, asserted so it cannot rot ─────────
    let r14 = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../benchmarks/benches/scale.rs"
    );
    let bench = std::fs::read_to_string(r14)
        .unwrap_or_else(|e| panic!("R14 scale bench missing at {r14}: {e}"));
    assert!(
        bench.contains("AIKOQL_BENCH_SCALE"),
        "R14 scale bench must carry the AIKOQL_BENCH_SCALE knob"
    );
    eprintln!(
        "[W31-SCALE-001 n=1M] pointer: {r14} — criterion, knob AIKOQL_BENCH_SCALE \
         (default 100k, 1M needs ~4GB), kernel-op level p50/p95/p99"
    );
}

/// W31-SCALE-001 follow-up (gap item 11): the ID-family flood. Names
/// like "cust0042" share their letter prefix across the whole family,
/// so the partial-prefix credit ranked every sibling for any probe —
/// measured with Customer0..CustomerN names: all 100k entities tied,
/// the RET-003 tie-group retraction rendered the whole group as
/// thousands of unbudgeted tokens (losses.md). The fix: an ID-style
/// token (letters then digits) earns no partial credit — the digits
/// carry the identity, so a word matching only the letters names no
/// member. Exact matches still rank; the asked member's fact must lead
/// the pack with no ambiguity group, and a family-only probe must
/// refuse (nothing ranks, entity-only → healthy empty).
#[test]
fn w31_scale_002_id_family_flood() {
    let n = 1000usize;
    let evd = evd();
    let mut entities = Vec::with_capacity(n);
    let mut facts = Vec::with_capacity(n);
    for k in 0..n {
        let name = format!("cust{k:04}");
        entities.push(EntityCandidate {
            name: name.clone(),
            type_hint: Some("Customer".into()),
            mentions: vec![name.clone()],
            confidence: 0.9,
            evidence: evd.clone(),
        });
        facts.push(FactCandidate {
            statement: format!("{name} tier is {}", tier(k)),
            entities: vec![name],
            confidence: 0.9,
            evidence: evd.clone(),
            snippet: None,
        });
    }
    let ir = KnowledgeIr {
        entities,
        relations: vec![],
        facts,
        events: vec![],
        temporal: vec![],
        document_id: Some("scale-synth-id".into()),
        source_revision: None,
        content_trust: None,
        page_count: 1,
        extractor: "scale-synth-id".into(),
    };

    let pkg = compile_context("What is the tier of cust0042?", &ir, BUDGET);
    let payload = render_context_markdown(&pkg);
    eprintln!(
        "[W31-SCALE-002 member-probe] tokens={} entities={} facts={} ambiguous={}",
        payload.len() / 4,
        pkg.entities.len(),
        pkg.facts.len(),
        pkg.ambiguous_entities.len(),
    );
    assert!(payload_has(&payload, "gold"), "the member's tier must pack");
    assert!(
        pkg.facts.first().map(|f| f.statement.contains("cust0042")) == Some(true),
        "the asked member's fact must lead the pack"
    );
    assert!(
        pkg.ambiguous_entities.is_empty(),
        "no family-wide tie group: {} siblings retracted as ambiguous",
        pkg.ambiguous_entities.len()
    );
    assert!(
        payload.len() / 4 <= 400,
        "payload must stay bounded: {} tokens",
        payload.len() / 4
    );

    // The family-only probe names no member: nothing ranks once the
    // partial credit dies, and the pack refuses rather than flood.
    let pkg2 = compile_context("What is the tier of cust?", &ir, BUDGET);
    let payload2 = render_context_markdown(&pkg2);
    eprintln!(
        "[W31-SCALE-002 family-probe] tokens={} entities={} facts={} ambiguous={}",
        payload2.len() / 4,
        pkg2.entities.len(),
        pkg2.facts.len(),
        pkg2.ambiguous_entities.len(),
    );
    assert!(
        payload2.trim().is_empty(),
        "family-only probe must refuse: {} tokens",
        payload2.len() / 4
    );
}
