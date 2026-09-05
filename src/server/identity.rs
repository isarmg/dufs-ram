#[cfg(test)]
use crate::utils::{decode_hex_to_slice, encode_hex};

use sha2::{Digest, Sha256};

/// Pseudonymous identifier used wherever state must be partitioned by an
/// authenticated owner without storing the account name directly.
///
/// This is an unkeyed SHA-256 digest, not a secrecy boundary: low-entropy
/// account names can be recovered by dictionary enumeration.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) struct OwnerId([u8; 32]);

impl OwnerId {
    pub(super) fn persistent(owner: &str) -> Self {
        Self::domain_separated(b"dufs-durable-owner-v1\0", owner)
    }

    pub(super) fn listing_snapshot(owner: &str) -> Self {
        Self::domain_separated(b"dufs-list-snapshot-owner-v1\0", owner)
    }

    fn domain_separated(domain: &[u8], owner: &str) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(domain);
        hasher.update(owner.as_bytes());
        Self(hasher.finalize().into())
    }

    pub(super) const fn into_bytes(self) -> [u8; 32] {
        self.0
    }

    #[cfg(test)]
    pub(super) fn to_hex(self) -> String {
        encode_hex(self.0)
    }

    #[cfg(test)]
    pub(super) fn from_hex(value: &str) -> Option<Self> {
        let mut bytes = [0_u8; 32];
        (value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            && decode_hex_to_slice(value, &mut bytes))
        .then_some(Self(bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persistent_owner_id_uses_the_current_durable_domain() {
        assert_eq!(
            OwnerId::persistent("alice").to_hex(),
            "35ad2994ff78c7cbd371449a5f087bd5bd23f766c5cd46825ca6be1a2addb5e4"
        );
    }

    #[test]
    fn hex_representation_round_trips_at_the_upload_boundary() {
        let owner = OwnerId::persistent("alice");
        assert_eq!(OwnerId::from_hex(&owner.to_hex()), Some(owner));
        assert_eq!(OwnerId::from_hex("NOT-A-DIGEST"), None);
    }

    #[test]
    fn listing_snapshot_ids_are_domain_separated() {
        assert_ne!(
            OwnerId::listing_snapshot("alice"),
            OwnerId::persistent("alice")
        );
    }
}
