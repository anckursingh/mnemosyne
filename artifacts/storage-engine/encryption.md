# Encryption Boundary (KSE-11, KSE-100..104)

Date: 2026-09-01 · `kse11_encryption_boundary.rs`

§31 canonical report. The phase is the REUSE gate: §17 requires reusing
the existing AIKOQL encryption architecture, not building a second
incompatible model. Result: zero crypto code in `aikoql-storage` — the
engine is byte-opaque; ciphertext rides the same enveloped WAL records as
everything else. The test imports only kernel security modules
(`EncryptedStore`, `Envelope`, `FieldCrypto`, `Crypto`,
`EncryptionPolicy`) and wraps the persistent engines with them.

## Gates (per gate: contract pin + aikoql == redb parity)

| gate | pin | result |
|---|---|---|
| KSE-100 | encrypted write/read round trip, then reopen (WAL replay) and read again | round trip holds; raw rows never hold plaintext at rest |
| KSE-101 | wrong key | fails closed — never garbage |
| KSE-102 | garbage planted where the ciphertext lives (bitrot / tamper) | deterministic decrypt error, never `Ok(garbage)` |
| KSE-103 | KEK rotation | re-wraps DEKs online (kek_id+1); pre-rotation ciphertext still decrypts |
| KSE-104 | crash during rotation (kernel dropped mid-life, rotation never persisted) | fresh kernel over a reopened engine decrypts the secret; no plaintext anywhere in the store |

The full `Summary` (all seven pins) is asserted `aikoql == redb` exactly —
the redb reference and the WAL engine behave identically through the
encryption boundary. The contract pins are additionally asserted on the
reference, so the phase pins the documented guarantees, not just parity.

## Scope, honestly

- **MemoryEngine**: out by design — no persistence, so no
  reopen/crash surface; the kernel's own suite covers
  EncryptedStore-over-memory (e04).
- **RocksDB**: no KSE-11 leg — `EncryptedStore` is engine-agnostic by §32
  (the same trait wraps any engine), but the phase pins the two
  production-relevant backends. RocksDB itself is feature-gated
  (`kse5-rocksdb`) and not a production default.
- **Field encryption** (KSE-103/104) exercises the kernel's
  Envelope/FieldCrypto path — engine-level and field-level encryption are
  one architecture, as §17 requires.
