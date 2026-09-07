# AIKOQL Storage Engine — MVP Certification Closure & TDD Development Plan

**Document Type:** Senior QA TDD Specification  
**Product:** AIKOQL  
**Component:** Native Storage Engine  
**Audience:** AI Coder / Rust Engineers / Storage Maintainers

## 1. Objective

Close the remaining evidence gaps before final MVP storage certification. This is not a request to add speculative database features.

Required workflow:

```text
QA Evidence → Coder Point of View → Confirm/Challenge → RED Test
→ Root Cause → Minimal GREEN Fix → Regression → Acceptance Gate
```

The coder must challenge this document where evidence supports disagreement.

---

# 2. Current QA Position

Existing TDD evidence demonstrates:

- StorageEngine contract conformance.
- WAL persistence and reopen recovery.
- Tested torn-write recovery.
- Checksum-based corruption detection.
- Concurrent read/write stress.
- Snapshot/restore coverage.
- Encryption boundary compatibility.
- Conformance against MemoryEngine, redb, RocksDB and AIKOQL storage.
- Property/stress testing.
- Resource measurement and comparative workload benchmarking.

Known architectural limitations already identified:

```text
Whole active state is replayed into memory.
WAL growth is currently unbounded.
The engine is memory-first.
Write durability is effectively serialized.
Compaction is not implemented.
```

These limitations are acceptable only if the MVP operational boundary is explicitly measured.

---

# 3. Mandatory Coder Response Protocol

Before implementing each finding, respond:

```markdown
# Finding: <ID>

## My Interpretation
Explain the concern in your own words.

## Confirm / Partially Confirm / Disagree
Choose one.

## Evidence From Current Implementation
Provide source files, functions, tests and execution evidence.

## My Point of View
Answer:
- Is the QA concern valid?
- Is current behavior already safe?
- Is the proposed test realistic?
- Is there a better test?
- What production failure does this prevent?

## Risk Classification
P0 / P1 / P2 / P3

## RED Test Proposal
Exact test name and expected failure.

## Minimal Fix
Smallest correct implementation change.

## Alternative Designs Considered

## Acceptance Criteria
Objective pass/fail conditions.
```

---

# 4. KSE-082B — Multi-Record Corruption With Valid Tail

## QA Evidence

Current evidence covers tested complete-record corruption and torn final writes.

The missing explicit scenario is:

```text
Record 100 → VALID
Record 101 → CORRUPTED
Record 102 → VALID
Record 103 → VALID
EOF        → VALID
```

This differs fundamentally from a torn tail.

## QA Concern

The engine must not silently:

```text
skip record 101 and continue
```

or:

```text
truncate valid acknowledged records after 101
```

without an explicit recovery policy.

## Required Coder Point of View

Answer:

1. Does replay stop immediately on a checksum mismatch in a non-tail record?
2. Can corruption ever be classified as a recoverable torn tail?
3. Does `open()` modify the WAL after detecting corruption?
4. Which policy is correct?

Choose and justify:

```text
A. Fail closed
B. Skip corrupted records
C. Truncate at corruption
D. Recover using secondary durable metadata
```

Disagreement with QA is allowed, but must be supported by tests and operational reasoning.

## RED Tests

### TEST-KSE-082B-01 — Middle Payload Corruption

```text
Given:
  200 valid WAL records

When:
  one payload byte in record 101 is modified

And:
  records 102-200 remain valid

Then:
  open() returns a corruption error

And:
  WAL size remains unchanged

And:
  records after corruption are not silently applied
```

### TEST-KSE-082B-02 — Middle Checksum Corruption

```text
Given:
  200 valid records

When:
  checksum bytes of record 101 are modified

Then:
  open() fails closed
  WAL remains unchanged
```

### TEST-KSE-082B-03 — Middle Header Corruption

Independently mutate:

```text
magic
version
record type
payload length
```

Expected:

```text
corruption error
no automatic truncation
no partial successful recovery
```

## Required Assertions

Assert:

```text
error type
error classification
WAL size
WAL hash before/after
recovered state
```

Do not only assert `open().is_err()`.

## Acceptance Gate

```text
Middle corruption → deterministic fail closed
Torn final append → deterministic recovery
Cases are distinguishable
Failed corruption open does not mutate WAL
```

---

# 5. KSE-120C — Writer Contention Scaling

## QA Evidence

Existing concurrency testing demonstrates correctness under concurrent activity.

That proves:

```text
no tested lost writes
no tested corruption
valid final state
```

It does not prove contention scalability.

The expected architecture is approximately:

```text
Writer A ─┐
Writer B ─┼→ serialized WAL path → fsync → apply
Writer N ─┘
```

The key unanswered question is:

> At what writer count does contention become dominant?

## Required Coder Point of View

Answer:

1. Is single-writer serialization intentional?
2. Where exactly is serialization enforced?
3. Which lock and critical section are used?
4. Is fsync inside the serialized section?
5. Would batching/group commit help?
6. Does AIKOQL MVP require high concurrent write throughput?

Separate:

```text
CURRENT MVP REQUIREMENT
```

from:

```text
FUTURE OPTIMIZATION
```

## Test Matrix

| Writers | Readers |
|---:|---:|
| 1 | 0 |
| 1 | 32 |
| 2 | 32 |
| 4 | 32 |
| 8 | 32 |
| 16 | 32 |
| 32 | 32 |

Keep workload, durability mode, key distribution, value size, hardware and release build constant.

## Metrics

```text
Writes/sec
Reads/sec
Write P50/P95/P99
Read P50/P95/P99
Lock/queue wait if measurable
WAL append time
fsync time
CPU
RSS
```

Do not invent metrics that cannot be measured.

## Correctness Assertions

After each scenario:

```text
expected acknowledged writes == recovered writes
```

Verify:

```text
no missing acknowledged writes
no duplicate commits
no WAL corruption
valid reopen
```

## RED Definition

The RED condition is:

```text
writer contention scaling evidence does not exist
```

Build the harness first. Optimize only if measured behavior violates an explicit AIKOQL MVP SLO.

## Acceptance

Minimum:

```text
100% acknowledged-write correctness at all tested writer counts
```

The coder must propose realistic performance SLOs based on AIKOQL workloads rather than generic database expectations.

---

# 6. KSE-142 — Recovery Scaling

## QA Evidence

Current evidence proves recovery semantics on smaller WAL sizes.

It does not establish scaling as:

```text
WAL size ↑
record count ↑
version history ↑
```

Because recovery replays WAL into memory, the expected cost may be linear.

The QA concern is not that linear recovery is automatically unacceptable.

The concern is:

> The scaling curve has not been measured and the MVP limit is not explicitly defined.

## Required Coder Point of View

Answer:

1. What is theoretical recovery complexity?
2. What dominates recovery?
3. Is recovery approximately linear?
4. What is the maximum intended MVP dataset?

Justify the target using AIKOQL workloads.

## Required Matrix

At minimum:

```text
1 MB WAL
10 MB WAL
100 MB WAL
```

If practical:

```text
1 GB WAL
```

For each report:

```text
record count
unique keys
overwrite ratio
delete ratio
average value size
```

## Metrics

```text
total open time
WAL replay time
time to first query
peak RSS
final RSS
```

## Correctness

After recovery validate:

```text
logical key count
reference key/value data
prefix scans
deletes
overwrites
```

## Acceptance

```text
100% semantic recovery
no corruption
no unexpected OOM within target MVP scale
scaling curve documented
```

The coder must propose an actual recovery SLO after measurement.

---

# 7. KSE-143 — Large Replay Resource Stability

## QA Evidence

Steady-state memory measurements exist, but startup peak memory can be materially higher because replay may create:

```text
WAL buffers
decoder allocations
temporary copies
BTreeMap growth
```

The deployment risk is peak memory, not only final memory.

## RED Test

```text
Generate target WAL
→ close engine
→ record baseline
→ open engine
→ measure peak RSS during replay
→ measure final RSS
→ execute first query
```

## Metrics

```text
WAL size
unique live keys
historical record count
peak RSS
final RSS
peak/final ratio
open time
```

## Required Acceptance

The final report must publish:

```text
Peak replay memory multiplier = Peak RSS / Final RSS
```

The coder must propose a target deployment memory requirement.

---

# 8. Regression Matrix

Run every applicable contract test against:

```text
AikoqlStorageEngine
MemoryEngine
redb
RocksDB
```

Distinguish:

```text
Storage contract tests
Backend conformance tests
AIKOQL WAL-specific tests
```

Do not force WAL-internal corruption tests onto backends that do not expose equivalent internals.

---

# 9. Test Rules

## Release Mode

Performance and scaling certification evidence MUST use:

```text
--release
```

## Determinism

Randomized workloads must print:

```text
seed
operation count
key count
value distribution
```

Failures must be reproducible.

## No Arbitrary Thresholds

Do not add timing assertions without justification.

## Separate Correctness and Performance

```text
Correctness = deterministic pass/fail
Performance = measured with environment metadata
```

## Preserve Evidence

Every test report must contain:

```text
test name
build mode
dataset
environment
results
PASS/FAIL
known limitations
```

---

# 10. Required Final Report

Create:

```text
docs/testing/STORAGE_ENGINE_MVP_CERTIFICATION_CLOSURE.md
```

## Executive Verdict

Choose exactly one:

```text
PASS
PASS WITH ACCEPTED LIMITATIONS
CONDITIONAL PASS
FAIL
```

## Results

| Test | Status | Evidence |
|---|---|---|
| KSE-082B | | |
| KSE-120C | | |
| KSE-142 | | |
| KSE-143 | | |

## Coder Point of View

For every finding include:

```text
QA concern
Coder interpretation
Agree / Partially Agree / Disagree
Evidence
Final decision
```

## Measured Operational Boundary

Document:

```text
recommended maximum MVP dataset
recommended memory
expected startup behavior
expected write concurrency
known unsupported scale
```

---

# 11. Senior QA Challenge Questions

The coder must explicitly answer:

## Q1 — Is fail-closed middle corruption always correct?

If not, propose a safer alternative and prove it with tests.

## Q2 — Is single-writer architecture actually a bottleneck for AIKOQL?

Answer from:

```text
Knowledge Object mutation patterns
agent workloads
repository ingestion
document ingestion
interactive updates
```

not generic database expectations.

## Q3 — Is full replay acceptable for MVP?

Provide:

```text
target dataset
measured recovery time
memory requirement
deployment profile
```

## Q4 — Should compaction block MVP release?

Choose:

```text
YES
or
NO
```

and justify with measured evidence.

---

# 12. Final Certification Gate

## Correctness

- [ ] KSE-082B passes.
- [ ] Middle corruption behavior is explicitly defined and tested.
- [ ] Torn final writes recover correctly.
- [ ] Failed corruption open does not silently mutate WAL.

## Concurrency

- [ ] KSE-120C completed.
- [ ] All tested writer counts preserve acknowledged-write correctness.
- [ ] Contention behavior measured.

## Recovery

- [ ] KSE-142 completed.
- [ ] Recovery scaling documented.
- [ ] Target MVP dataset defined.

## Resources

- [ ] KSE-143 completed.
- [ ] Peak replay RSS measured.
- [ ] Final RSS measured.
- [ ] Deployment memory requirement documented.

## Evidence

- [ ] Release-mode results.
- [ ] Environment documented.
- [ ] Seeds reproducible.
- [ ] No unsupported performance claims.

---

# 13. Final QA Position

The current engine should be certified, if these gates pass, as:

```text
AIKOQL MVP Native Storage Backend
```

It should not yet be marketed internally or externally as:

```text
Universal General-Purpose Database Storage Engine
```

Current expected strengths:

```text
Agent memory
Knowledge Objects
Repository knowledge
Code intelligence
Document knowledge
Ontology-driven applications
Medium-scale knowledge graphs
Local/embedded deployments
```

Not yet certified for:

```text
Unbounded historical datasets
Multi-terabyte storage
Very high concurrent writes
General-purpose OLTP workloads
```

The goal is not to prove perfection.

The goal is to prove:

> What AIKOQL Storage Engine is safe and suitable for today, using measurable evidence.

---

# Appendix — Mandatory TDD Loop

```text
QA concern
   ↓
Coder Point of View
   ↓
Evidence review
   ↓
RED test
   ↓
Failure confirmed
   ↓
Minimal implementation
   ↓
GREEN
   ↓
Regression
   ↓
Release measurement
   ↓
Certification decision
```

Every implementation change must answer:

```text
What failure does this prevent?
How does the RED test prove the gap?
What is the smallest correct fix?
What evidence proves the fix?
```
