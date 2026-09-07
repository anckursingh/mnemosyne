//! Wave 3.1 (MVP-QA-003A) — W31-REAL-001, the gated real-LLM leg.
//! The same 50-task × 5-repetition agent chain as the deterministic sim,
//! with a live generator in place of the payload echo. SKIPs without
//! `AIKOQL_ANSWER_MODEL` (the e2e_answer_quality convention — CI never
//! dials out).
//!
//! ```text
//! $env:AIKOQL_ANSWER_MODEL = "qwen2.5:3b"
//! cargo test --features answer_gen --test wave31_agent_sim_llm -- --nocapture
//! ```
//!
//! No asserts on generated answers — a model's answers are what they
//! are; the harness prints per-class scores, per-rep W7 (the
//! repeatability the spec asks for, real this time), groundedness,
//! unsupported-token counts (answers may and will carry tokens the
//! payload lacks — that column is measured here, honestly), and the
//! W11 false-confidence / refusal columns. Runtime note: 250–500 local
//! generations.
#![cfg(feature = "answer_gen")]

mod common;

use aikoql_ingestion::{merge_knowledge_ir, KnowledgeIr, MockEmbeddingProvider};
use common::trackb::corpus;
use common::wave31_sim::{
    agent_policy, aikoql_context, generate, rag_context, sample_tasks, union_docs, union_questions,
    unsupported_tokens, win_zone, AgentOutcome, REPS,
};

const SYSTEM: &str = "You are a support agent with access to a knowledge store. \
    Answer the task using ONLY the evidence provided. Cite sources where possible. \
    If the evidence is insufficient, say you do not know.";

/// 50 × 5 with a live generator, both treatments, judged by the same
/// win-zone. Prints only; the deterministic sim owns the asserts.
#[test]
fn w31_real_001_llm_leg() {
    let Some(model) = std::env::var("AIKOQL_ANSWER_MODEL").ok() else {
        eprintln!(
            "[W31-REAL-001-LLM] SKIP — set AIKOQL_ANSWER_MODEL (and optionally \
             AIKOQL_ANSWER_ENDPOINT) to run the real-LLM leg"
        );
        return;
    };
    // ollama's chat API lives at /api/chat; the bare host answers 405
    // (the pre-fix run measured 500/500 generation failures against it).
    let endpoint = std::env::var("AIKOQL_ANSWER_ENDPOINT")
        .unwrap_or_else(|_| "http://127.0.0.1:11434/api/chat".to_string());

    let provider = MockEmbeddingProvider::new();
    let docs = union_docs();
    let corpus = corpus(&docs);
    let irs: Vec<KnowledgeIr> = docs.iter().map(|d| d.ir.clone()).collect();
    let merged = merge_knowledge_ir(&irs);
    let all = union_questions();
    let tasks = sample_tasks(&all);
    assert_eq!(tasks.len(), 50);

    let mut a_score = 0usize;
    let mut r_score = 0usize;
    let mut a_grounded = 0usize;
    let mut r_grounded = 0usize;
    let mut a_unsupported = 0usize;
    let mut r_unsupported = 0usize;
    let mut a_answers = 0usize;
    let mut r_answers = 0usize;
    let mut a_retries = 0usize;
    let mut r_retries = 0usize;
    let mut a_w11_fc = 0usize;
    let mut r_w11_fc = 0usize;
    let mut w7_per_rep: Vec<(usize, usize)> = Vec::new();

    for rep in 0..REPS {
        let (mut a_w7, mut r_w7) = (0usize, 0usize);
        for (qi, q) in tasks.iter().enumerate() {
            for (name, ctx, score, grounded, unsupported, answers, retries, fc, w7) in [
                (
                    "aikoql",
                    aikoql_context(q, &merged),
                    &mut a_score,
                    &mut a_grounded,
                    &mut a_unsupported,
                    &mut a_answers,
                    &mut a_retries,
                    &mut a_w11_fc,
                    &mut a_w7,
                ),
                (
                    "rag",
                    rag_context(q, &corpus, &provider),
                    &mut r_score,
                    &mut r_grounded,
                    &mut r_unsupported,
                    &mut r_answers,
                    &mut r_retries,
                    &mut r_w11_fc,
                    &mut r_w7,
                ),
            ] {
                match agent_policy(q, &ctx) {
                    AgentOutcome::Refuse(reason) => {
                        eprintln!("[W31-LLM rep{rep} Q{qi} {name}] refuse: {reason}");
                    }
                    AgentOutcome::Answer(_) => {
                        let prompt =
                            format!("Task: {}\n\nEvidence:\n{}\n\nAnswer:", q.text, ctx.payload);
                        // Generation retry — the agent-layer retry surface
                        // the deterministic slice structurally lacks.
                        let mut gen = generate(&endpoint, &model, SYSTEM, &prompt);
                        if gen.is_none() {
                            *retries += 1;
                            gen = generate(&endpoint, &model, SYSTEM, &prompt);
                        }
                        match gen {
                            Some(answer) => {
                                *answers += 1;
                                let s = win_zone(&answer, q);
                                *score += s;
                                if q.class == "W7" {
                                    *w7 += s;
                                }
                                if common::tokens(&answer).iter().any(|t| t == "kb") {
                                    *grounded += 1;
                                }
                                *unsupported += unsupported_tokens(&answer, &ctx.payload);
                                if q.class == "W11"
                                    && ctx.status == aikoql_ingestion::RetrievalStatus::Healthy
                                    && !ctx.payload.trim().is_empty()
                                {
                                    *fc += 1;
                                }
                                eprintln!(
                                    "[W31-LLM rep{rep} Q{qi} {} {}] score={}/2 unsupported={}",
                                    name,
                                    q.class,
                                    s,
                                    unsupported_tokens(&answer, &ctx.payload)
                                );
                            }
                            None => {
                                eprintln!("[W31-LLM rep{rep} Q{qi} {name}] generation failed");
                            }
                        }
                    }
                }
            }
        }
        w7_per_rep.push((a_w7, r_w7));
        eprintln!(
            "[W31-LLM rep{rep}] cumulative aikoql {} rag {} — W7 aikoql {} rag {}",
            a_score, r_score, a_w7, r_w7
        );
    }

    eprintln!(
        "[W31-LLM] totals: aikoql {a_score}/{} (grounded {a_grounded}/{a_answers}, \
         unsupported tokens {a_unsupported}, generation retries {a_retries}, \
         W11 false-confidence {a_w11_fc}) vs \
         rag {r_score} (grounded {r_grounded}/{r_answers}, unsupported {r_unsupported}, \
         generation retries {r_retries}, W11 false-confidence {r_w11_fc})",
        tasks.len() * REPS * 2,
    );
    eprintln!("[W31-LLM] per-rep W7: {w7_per_rep:?}");
}
