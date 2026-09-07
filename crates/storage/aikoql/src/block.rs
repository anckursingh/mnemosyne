//! Sorted immutable block (KSE-4, MRFC-KSE-001 §10).
//!
//! Physical layout:
//!
//! ```text
//! magic("AKBK")(4) version(1) flags(1) n_keys(4 BE)
//!   directory: (klen(4 BE), key, voffset(4 BE), vlen(4 BE)) × n_keys
//!              keys strictly ascending; voffset is within the value area
//! value area: values concatenated in directory order
//! checksum(8): first 8 bytes of sha256 over everything before it
//! ```
//!
//! A block is the compaction unit: once the WAL is compacted into blocks,
//! reopen loads blocks instead of replaying the full write history. Same
//! checksum primitive as the record envelope; flags bit 0 is reserved for
//! encryption (KSE-11).

use super::Cursor;
use aikoql_kernel::knowledge::kom::{sha256, KError, KResult};

const MAGIC: &[u8; 4] = b"AKBK";
const FORMAT_VERSION: u8 = 1;
const HEADER_LEN: usize = 10; // magic(4) + version(1) + flags(1) + n_keys(4)
const CHECKSUM_LEN: usize = 8;

fn corrupt(what: &str) -> KError {
    KError::Store(format!("aikoql-storage: corrupt block: {}", what))
}

/// Immutable, key-sorted KV block with a checksummed physical format.
#[derive(Debug)]
pub struct Block {
    entries: Vec<(Vec<u8>, Vec<u8>)>, // sorted by key, unique
}

impl Block {
    /// Build a block from any pair order: keys are sorted; duplicate keys
    /// collapse to the last value (WriteBatch put semantics, KSE-006).
    pub fn from_pairs(mut pairs: Vec<(Vec<u8>, Vec<u8>)>) -> Block {
        pairs.sort_by(|a, b| a.0.cmp(&b.0));
        let mut entries: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(pairs.len());
        for (k, v) in pairs {
            match entries.last_mut() {
                Some(last) if last.0 == k => last.1 = v,
                _ => entries.push((k, v)),
            }
        }
        Block { entries }
    }

    /// Serialize to the physical layout above.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(MAGIC);
        out.push(FORMAT_VERSION);
        out.push(0); // flags — bit 0 reserved for encryption (KSE-11)
        out.extend_from_slice(&(self.entries.len() as u32).to_be_bytes());
        // Directory; voffset is relative to the value area.
        // ponytail: u32 offsets cap a block at 4 GiB — compaction splits before then.
        let mut voff: u32 = 0;
        for (k, v) in &self.entries {
            out.extend_from_slice(&(k.len() as u32).to_be_bytes());
            out.extend_from_slice(k);
            out.extend_from_slice(&voff.to_be_bytes());
            out.extend_from_slice(&(v.len() as u32).to_be_bytes());
            voff += v.len() as u32;
        }
        for (_, v) in &self.entries {
            out.extend_from_slice(v);
        }
        let ck = sha256(&out);
        out.extend_from_slice(&ck[..CHECKSUM_LEN]);
        out
    }

    /// Parse a block; any corruption, truncation, or incompatibility fails
    /// closed — no corrupted data is ever returned as valid.
    pub fn decode(bytes: &[u8]) -> KResult<Block> {
        if bytes.len() < HEADER_LEN + CHECKSUM_LEN {
            return Err(corrupt("too short"));
        }
        let body = &bytes[..bytes.len() - CHECKSUM_LEN];
        let stored = &bytes[bytes.len() - CHECKSUM_LEN..];
        let computed = sha256(body);
        if stored != &computed[..CHECKSUM_LEN] {
            return Err(corrupt("checksum mismatch"));
        }
        let mut c = Cursor { b: body, pos: 0 };
        if c.take(4)? != MAGIC {
            return Err(corrupt("bad magic"));
        }
        let version = c.take(1)?[0];
        if version != FORMAT_VERSION {
            return Err(KError::Store(format!(
                "aikoql-storage: unsupported block version {} (this build supports {})",
                version, FORMAT_VERSION
            )));
        }
        let _flags = c.take(1)?[0];
        let n = c.u32_be()? as usize;
        let mut dir: Vec<(Vec<u8>, u32, usize)> = Vec::with_capacity(n);
        for _ in 0..n {
            let klen = c.u32_be()? as usize;
            let k = c.take(klen)?.to_vec();
            let voff = c.u32_be()?;
            let vlen = c.u32_be()? as usize;
            dir.push((k, voff, vlen));
        }
        // KSE-031: strictly ascending — binary search below depends on it.
        if dir.windows(2).any(|w| w[0].0 >= w[1].0) {
            return Err(corrupt("directory not sorted"));
        }
        let value_area = &body[c.pos..];
        let mut entries = Vec::with_capacity(n);
        for (k, voff, vlen) in dir {
            let end = voff as usize + vlen;
            if end > value_area.len() {
                return Err(corrupt("value out of bounds"));
            }
            entries.push((k, value_area[voff as usize..end].to_vec()));
        }
        Ok(Block { entries })
    }

    /// Point lookup by binary search over the sorted keys.
    pub fn get(&self, key: &[u8]) -> Option<&[u8]> {
        let i = self
            .entries
            .binary_search_by(|(k, _)| k.as_slice().cmp(key))
            .ok()?;
        Some(&self.entries[i].1)
    }

    /// All pairs whose key starts with `prefix`, ascending.
    pub fn prefix(&self, prefix: &[u8]) -> Vec<(&[u8], &[u8])> {
        let start = self.entries.partition_point(|(k, _)| k.as_slice() < prefix);
        self.entries[start..]
            .iter()
            .take_while(|(k, _)| k.starts_with(prefix))
            .map(|(k, v)| (k.as_slice(), v.as_slice()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs() -> Vec<(Vec<u8>, Vec<u8>)> {
        vec![
            (b"b".to_vec(), vec![2]),
            (b"a".to_vec(), vec![1]),
            (b"d".to_vec(), vec![4]),
            (b"c".to_vec(), vec![3]),
        ]
    }

    /// KSE-030 — write/read round trip: every record survives, in any input
    /// order, including empty and single-pair blocks, from memory AND file.
    #[test]
    fn kse030_block_round_trip() {
        let inputs: Vec<Vec<(Vec<u8>, Vec<u8>)>> =
            vec![vec![], vec![(b"only".to_vec(), vec![9])], pairs()];
        for input in inputs {
            let b = Block::from_pairs(input.clone());
            let d = Block::decode(&b.encode()).unwrap();
            let mut expected = input;
            expected.sort_by(|x, y| x.0.cmp(&y.0));
            let got: Vec<(Vec<u8>, Vec<u8>)> = d
                .entries
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            assert_eq!(got, expected);
        }

        // Literal write/read: through a file.
        let b = Block::from_pairs(pairs());
        let p = std::env::temp_dir().join(format!("aikoql_kse4_unit_{}.blk", std::process::id()));
        std::fs::write(&p, b.encode()).unwrap();
        let d = Block::decode(&std::fs::read(&p).unwrap()).unwrap();
        assert_eq!(d.get(b"b"), Some(&[2][..]));
        let _ = std::fs::remove_file(&p);
    }

    /// KSE-031 — all keys strictly ordered; a checksum-valid block whose
    /// directory is not sorted is rejected (lookups depend on the order).
    #[test]
    fn kse031_sorted_keys() {
        let b = Block::from_pairs(pairs());
        let d = Block::decode(&b.encode()).unwrap();
        assert!(d.entries.windows(2).all(|w| w[0].0 < w[1].0));

        // Hand-craft an authentic-checksum block with swapped directory keys.
        let mut bytes =
            Block::from_pairs(vec![(b"b".to_vec(), vec![1]), (b"a".to_vec(), vec![2])]).encode();
        // Directory: 1-byte keys, entry = klen(4) + key(1) + voffset(4) + vlen(4).
        bytes.swap(HEADER_LEN + 4, HEADER_LEN + 4 + 13);
        let ck = sha256(&bytes[..bytes.len() - CHECKSUM_LEN]);
        let n = bytes.len();
        bytes[n - CHECKSUM_LEN..].copy_from_slice(&ck[..CHECKSUM_LEN]);
        let err = Block::decode(&bytes).unwrap_err();
        assert!(
            format!("{err}").contains("directory not sorted"),
            "got: {err}"
        );
    }

    /// KSE-032 — point lookup by binary search: every hit plus the three
    /// miss shapes (before all, between keys, after all); last put wins.
    #[test]
    fn kse032_point_lookup() {
        let b = Block::from_pairs(pairs());
        assert_eq!(b.get(b"a"), Some(&[1][..]));
        assert_eq!(b.get(b"b"), Some(&[2][..]));
        assert_eq!(b.get(b"c"), Some(&[3][..]));
        assert_eq!(b.get(b"d"), Some(&[4][..]));
        assert_eq!(b.get(b"0"), None);
        assert_eq!(b.get(b"ab"), None);
        assert_eq!(b.get(b"z"), None);
        let d = Block::from_pairs(vec![(b"k".to_vec(), vec![1]), (b"k".to_vec(), vec![2])]);
        assert_eq!(d.get(b"k"), Some(&[2][..]));
        assert_eq!(d.entries.len(), 1);
    }

    /// KSE-033 — prefix range: in-range keys only, ascending; keys that
    /// share the first bytes but diverge, and shorter keys, are excluded.
    #[test]
    fn kse033_prefix_range() {
        let b = Block::from_pairs(vec![
            (b"ab".to_vec(), vec![1]),
            (b"abc".to_vec(), vec![2]),
            (b"abd".to_vec(), vec![3]),
            (b"ac".to_vec(), vec![4]), // diverges at the second byte
            (b"a".to_vec(), vec![0]),  // shorter than the prefix
            (b"b".to_vec(), vec![5]),  // after the range
        ]);
        let got = b.prefix(b"ab");
        for (k, _) in &got {
            assert!(k.starts_with(b"ab"), "out-of-range key: {k:?}");
        }
        let keys: Vec<&[u8]> = got.iter().map(|(k, _)| *k).collect();
        assert_eq!(keys, vec![&b"ab"[..], b"abc", b"abd"]);
        assert_eq!(b.prefix(b"zz"), Vec::<(&[u8], &[u8])>::new());
        assert_eq!(b.prefix(b"").len(), 6); // empty prefix = whole block
    }
}
