#[cfg(test)]
use crate::utils::{decode_hex_to_slice, encode_hex};

use sha2::{Digest, Sha256};

/// Opaque, non-reversible identifier used wherever durable state must be
/// partitioned by authenticated owner without persisting the account name.
///
/// `persistent` deliberately keeps the historical unscoped SHA-256 wire
/// representation: operation, upload, and purge rows written by older
/// versions must remain addressable after an upgrade. Ephemeral consumers can
/// opt into a domain-separated digest instead.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) struct OwnerId([u8; 32]);

impl OwnerId {
    pub(super) fn persistent(owner: &str) -> Self {
        Self(Sha256::digest(owner.as_bytes()).into())
    }

    pub(super) fn listing_snapshot(owner: &str) -> Self {
        Self::domain_separated(b"dufs-list-snapshot-owner-v1\0", owner)
    }

    pub(super) fn login_throttle(owner: &str) -> Self {
        Self::domain_separated(b"dufs-login-throttle-owner-v1\0", owner)
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
    fn persistent_owner_id_preserves_the_existing_digest() {
        assert_eq!(
            OwnerId::persistent("alice").to_hex(),
            "2bd806c97f0e00af1a1fc3328fa763a9269723c8db8fac4f93af71db186d6e90"
        );
    }

    #[test]
    fn hex_representation_round_trips_at_the_legacy_upload_boundary() {
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
        assert_ne!(
            OwnerId::login_throttle("alice"),
            OwnerId::listing_snapshot("alice")
        );
    }
}
