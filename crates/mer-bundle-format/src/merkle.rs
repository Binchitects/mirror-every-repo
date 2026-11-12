use mer_core::{Digest, Hasher};

/// Proof depth for `n` segments: the tree is padded to a power of two and
/// always has at least one level, so every proof has the same length.
pub fn depth_for(n: u32) -> u8 {
    let mut d = 1u8;
    while (1u64 << d) < n as u64 {
        d += 1;
    }
    d
}

/// `SHA256(0x00 || index || segment)`. The index is hashed in, so a valid
/// segment cannot be replayed at another position.
pub fn leaf_hash(index: u32, segment: &[u8]) -> Digest {
    let mut h = Hasher::new();
    h.update(&[0x00]);
    h.update(&index.to_be_bytes());
    h.update(segment);
    h.finish()
}

fn node_hash(left: &Digest, right: &Digest) -> Digest {
    let mut h = Hasher::new();
    h.update(&[0x01]);
    h.update(&left.0);
    h.update(&right.0);
    h.finish()
}

pub struct MerkleTree {
    /// levels[0] = leaves (padded with zero digests), last level = [root].
    levels: Vec<Vec<Digest>>,
}

impl MerkleTree {
    pub fn from_leaves(mut leaves: Vec<Digest>) -> Self {
        assert!(!leaves.is_empty());
        let depth = depth_for(leaves.len() as u32);
        leaves.resize(1usize << depth, Digest::ZERO);
        let mut levels = vec![leaves];
        for _ in 0..depth {
            let prev = levels.last().unwrap();
            let next: Vec<Digest> = prev.as_chunks::<2>().0.iter().map(|p| node_hash(&p[0], &p[1])).collect();
            levels.push(next);
        }
        MerkleTree { levels }
    }

    pub fn depth(&self) -> u8 {
        (self.levels.len() - 1) as u8
    }

    pub fn root(&self) -> Digest {
        self.levels.last().unwrap()[0]
    }

    /// Sibling hashes from the leaf up.
    pub fn proof(&self, index: u32) -> Vec<Digest> {
        let mut i = index as usize;
        let mut out = Vec::with_capacity(self.depth() as usize);
        for level in &self.levels[..self.levels.len() - 1] {
            out.push(level[i ^ 1]);
            i >>= 1;
        }
        out
    }
}

pub fn verify_proof(root: &Digest, index: u32, leaf: Digest, proof: &[Digest]) -> bool {
    if proof.len() >= 32 || (index as u64) >> proof.len() != 0 {
        return false;
    }
    let mut i = index;
    let mut acc = leaf;
    for sib in proof {
        acc = if i & 1 == 0 { node_hash(&acc, sib) } else { node_hash(sib, &acc) };
        i >>= 1;
    }
    acc == *root
}
