//! Who a client is across connections: an ed25519 key the client makes once.
//!
//! A client proves it holds its key by signing the connection's TLS
//! [`exporter`](crate::tls::exporter) value in its Hello. Both ends derive that
//! value and nobody else can, so a proof can't be replayed on another
//! connection or to another server.

use std::fmt;
use std::str::FromStr;

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::de::{self, SeqAccess, Visitor};
use serde::ser::SerializeTuple;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::update::{from_hex, to_hex};

/// A client's public key. Bans and moderator lists refer to people by it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Identity(pub [u8; 32]);

impl Identity {
    /// The full key in hex, as written in the server's files.
    pub fn to_hex(&self) -> String {
        to_hex(&self.0)
    }
}

/// The first 8 bytes in hex: enough to tell people apart on screen.
impl fmt::Display for Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&to_hex(&self.0[..8]))
    }
}

impl fmt::Debug for Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Identity({})", self.to_hex())
    }
}

impl FromStr for Identity {
    type Err = &'static str;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bytes = from_hex(s).ok_or("an identity is 64 hex digits")?;
        Ok(Self(bytes.try_into().map_err(|_| "an identity is 64 hex digits")?))
    }
}

/// Hex in text formats like TOML, 32 raw bytes on the wire.
impl Serialize for Identity {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        if s.is_human_readable() { s.serialize_str(&self.to_hex()) } else { self.0.serialize(s) }
    }
}

impl<'de> Deserialize<'de> for Identity {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        if d.is_human_readable() {
            String::deserialize(d)?.parse().map_err(de::Error::custom)
        } else {
            <[u8; 32]>::deserialize(d).map(Self)
        }
    }
}

/// Sent in Hello: a public key and its signature over the connection's exporter value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdentityProof {
    pub public_key: [u8; 32],
    #[serde(with = "bytes64")]
    pub signature: [u8; 64],
}

impl IdentityProof {
    /// The identity this proves on the connection with `exporter`, if the signature holds.
    pub fn verify(&self, exporter: &[u8; 32]) -> Option<Identity> {
        let key = VerifyingKey::from_bytes(&self.public_key).ok()?;
        key.verify_strict(exporter, &Signature::from_bytes(&self.signature)).ok()?;
        Some(Identity(self.public_key))
    }
}

/// The secret half of an identity: 32 random bytes, kept in the client's settings.
#[derive(Clone, PartialEq, Eq)]
pub struct IdentityKey(SigningKey);

impl IdentityKey {
    pub fn from_seed(seed: [u8; 32]) -> Self {
        Self(SigningKey::from_bytes(&seed))
    }

    pub fn identity(&self) -> Identity {
        Identity(self.0.verifying_key().to_bytes())
    }

    /// Proves this identity on the connection whose exporter value this is.
    pub fn prove(&self, exporter: &[u8; 32]) -> IdentityProof {
        IdentityProof { public_key: self.identity().0, signature: self.0.sign(exporter).to_bytes() }
    }
}

/// Never prints the secret.
impl fmt::Debug for IdentityKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "IdentityKey(for {})", self.identity())
    }
}

/// The seed in hex.
impl Serialize for IdentityKey {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&to_hex(self.0.as_bytes()))
    }
}

impl<'de> Deserialize<'de> for IdentityKey {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let seed = from_hex(&String::deserialize(d)?).and_then(|b| <[u8; 32]>::try_from(b).ok());
        seed.map(Self::from_seed).ok_or_else(|| de::Error::custom("an identity key is 64 hex digits"))
    }
}

/// Serde implements arrays only up to 32 elements; a signature has 64 bytes.
mod bytes64 {
    use super::*;

    pub fn serialize<S: Serializer>(bytes: &[u8; 64], s: S) -> Result<S::Ok, S::Error> {
        let mut tuple = s.serialize_tuple(64)?;
        for b in bytes {
            tuple.serialize_element(b)?;
        }
        tuple.end()
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 64], D::Error> {
        struct Bytes;
        impl<'de> Visitor<'de> for Bytes {
            type Value = [u8; 64];
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("64 bytes")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<[u8; 64], A::Error> {
                let mut out = [0u8; 64];
                for (i, b) in out.iter_mut().enumerate() {
                    *b = seq.next_element()?.ok_or_else(|| de::Error::invalid_length(i, &self))?;
                }
                Ok(out)
            }
        }
        d.deserialize_tuple(64, Bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_proof_holds_only_for_its_own_exporter() {
        let key = IdentityKey::from_seed([7; 32]);
        let proof = key.prove(&[1; 32]);
        assert_eq!(proof.verify(&[1; 32]), Some(key.identity()));
        assert_eq!(proof.verify(&[2; 32]), None, "replayed on another connection");

        let other = IdentityKey::from_seed([8; 32]);
        let stolen = IdentityProof { public_key: other.identity().0, ..proof.clone() };
        assert_eq!(stolen.verify(&[1; 32]), None, "someone else's key with this signature");

        let wire = postcard::to_stdvec(&proof).unwrap();
        assert_eq!(wire.len(), 96);
        assert_eq!(postcard::from_bytes::<IdentityProof>(&wire).unwrap(), proof);
    }

    #[test]
    fn identities_are_hex_in_text_and_short_on_screen() {
        let id = IdentityKey::from_seed([7; 32]).identity();
        assert_eq!(id.to_string().len(), 16);
        assert!(id.to_hex().starts_with(&id.to_string()));
        assert_eq!(id.to_hex().parse::<Identity>(), Ok(id));
        assert!(id.to_string().parse::<Identity>().is_err());

        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct File {
            who: Identity,
            key: IdentityKey,
        }
        let file = File { who: id, key: IdentityKey::from_seed([7; 32]) };
        let text = toml::to_string(&file).unwrap();
        assert!(text.contains(&id.to_hex()), "{text}");
        assert_eq!(toml::from_str::<File>(&text).unwrap(), file);
        assert_eq!(postcard::to_stdvec(&id).unwrap().len(), 32);
        assert!(!format!("{:?}", file.key).contains(&to_hex(&[7; 32])), "Debug leaks the secret");
    }
}
