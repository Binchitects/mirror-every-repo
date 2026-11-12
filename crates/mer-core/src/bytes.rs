use crate::CoreError;
use sha2::{Digest as _, Sha256};

macro_rules! bytes_newtype {
    ($(#[$m:meta])* $name:ident, $n:expr) => {
        $(#[$m])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(pub [u8; $n]);

        impl $name {
            pub fn to_hex(&self) -> String {
                hex::encode(self.0)
            }
            pub fn from_hex(s: &str) -> Result<Self, CoreError> {
                let v = hex::decode(s.trim()).map_err(|_| CoreError::BadHex)?;
                Self::from_slice(&v)
            }
            pub fn from_slice(b: &[u8]) -> Result<Self, CoreError> {
                let a: [u8; $n] = b
                    .try_into()
                    .map_err(|_| CoreError::BadLength { expected: $n, got: b.len() })?;
                Ok(Self(a))
            }
            pub fn as_bytes(&self) -> &[u8] {
                &self.0
            }
        }
        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.to_hex())
            }
        }
        impl std::fmt::Debug for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}({})", stringify!($name), self.to_hex())
            }
        }
        impl serde::Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_bytes(&self.0)
            }
        }
        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                struct V;
                impl<'de> serde::de::Visitor<'de> for V {
                    type Value = $name;
                    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                        write!(f, "{} bytes", $n)
                    }
                    fn visit_bytes<E: serde::de::Error>(self, v: &[u8]) -> Result<$name, E> {
                        $name::from_slice(v).map_err(E::custom)
                    }
                    fn visit_seq<A: serde::de::SeqAccess<'de>>(
                        self,
                        mut seq: A,
                    ) -> Result<$name, A::Error> {
                        let mut out = [0u8; $n];
                        for (i, slot) in out.iter_mut().enumerate() {
                            *slot = seq
                                .next_element()?
                                .ok_or_else(|| serde::de::Error::invalid_length(i, &self))?;
                        }
                        Ok($name(out))
                    }
                }
                d.deserialize_bytes(V)
            }
        }
    };
}

bytes_newtype!(
    /// SHA-256 of raw content: the key of the content-addressed store.
    Digest,
    32
);
bytes_newtype!(
    /// Chained snapshot root. Identifies a repo state on both sides of the gap.
    SnapshotId,
    32
);
bytes_newtype!(BundleId, 32);
bytes_newtype!(ChannelId, 8);
bytes_newtype!(
    /// Ed25519 public key.
    PubKey,
    32
);

impl Digest {
    pub const ZERO: Digest = Digest([0; 32]);
    pub fn of(data: &[u8]) -> Self {
        Digest(Sha256::digest(data).into())
    }
}

/// Incremental SHA-256.
#[derive(Clone, Default)]
pub struct Hasher(Sha256);

impl Hasher {
    pub fn new() -> Self {
        Self(Sha256::new())
    }
    pub fn update(&mut self, data: &[u8]) {
        self.0.update(data);
    }
    pub fn finish(self) -> Digest {
        Digest(self.0.finalize().into())
    }
}
