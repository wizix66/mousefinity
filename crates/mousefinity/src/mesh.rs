//! Mesh membership: one shared token = one tenant.
//!
//! Every host holding the same 32-byte mesh secret is a member. Membership
//! is proven with a keyed hash bound to the two authenticated endpoint ids
//! of the connection, so the secret itself never crosses the wire, proofs
//! cannot be replayed between other machine pairs, and a relay shared with
//! strangers learns nothing: tenancy is enforced by cryptography, not by
//! server configuration.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Public identifier of a mesh (safe to reveal; it does not leak the secret).
pub fn mesh_id(secret: &[u8; 32]) -> [u8; 32] {
    blake3::derive_key("mousefinity mesh id v1", secret)
}

/// Proof that the holder of `secret` is the dialer on a connection between
/// `dialer` and `acceptor` (iroh endpoint ids).
pub fn proof(secret: &[u8; 32], dialer: &[u8; 32], acceptor: &[u8; 32]) -> [u8; 32] {
    let key = blake3::derive_key("mousefinity mesh proof v1", secret);
    let mut h = blake3::Hasher::new_keyed(&key);
    h.update(dialer);
    h.update(acceptor);
    *h.finalize().as_bytes()
}

/// Everything a new machine needs to join: the secret plus one existing
/// member to bootstrap from. Share it like a Wi-Fi password.
#[derive(Debug, Serialize, Deserialize)]
pub struct Ticket {
    pub secret: [u8; 32],
    pub bootstrap_id: [u8; 32],
    pub bootstrap_name: String,
}

const TICKET_PREFIX: &str = "mfmesh";

pub fn encode_ticket(t: &Ticket) -> String {
    let bytes = postcard::to_stdvec(t).expect("ticket serializes");
    format!(
        "{TICKET_PREFIX}{}",
        data_encoding::BASE32_NOPAD
            .encode(&bytes)
            .to_ascii_lowercase()
    )
}

pub fn decode_ticket(s: &str) -> Result<Ticket> {
    let rest = s
        .trim()
        .strip_prefix(TICKET_PREFIX)
        .context("not a mesh ticket (it should start with `mfmesh`)")?;
    let bytes = data_encoding::BASE32_NOPAD
        .decode(rest.to_ascii_uppercase().as_bytes())
        .context("ticket is corrupt (bad encoding)")?;
    postcard::from_bytes(&bytes).context("ticket is corrupt (bad contents)")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticket_roundtrip() {
        let t = Ticket {
            secret: [7u8; 32],
            bootstrap_id: [9u8; 32],
            bootstrap_name: "desktop".into(),
        };
        let s = encode_ticket(&t);
        assert!(s.starts_with("mfmesh"));
        let back = decode_ticket(&s).unwrap();
        assert_eq!(back.secret, t.secret);
        assert_eq!(back.bootstrap_id, t.bootstrap_id);
        assert_eq!(back.bootstrap_name, "desktop");
    }

    #[test]
    fn proof_binds_pair_and_direction() {
        let secret = [1u8; 32];
        let a = [2u8; 32];
        let b = [3u8; 32];
        let p = proof(&secret, &a, &b);
        assert_eq!(p, proof(&secret, &a, &b));
        assert_ne!(p, proof(&secret, &b, &a), "direction must matter");
        assert_ne!(p, proof(&[9u8; 32], &a, &b), "secret must matter");
    }
}
