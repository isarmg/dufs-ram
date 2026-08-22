use super::{DirectorySnapshot, ListingError, ListingProblem, ListingResult, PathItem};
use crate::server::identity::OwnerId;
use anyhow::{Result, anyhow};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use std::{
    collections::{HashMap, VecDeque},
    ops::Range,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};

pub(super) const MAX_CACHED_LIST_SNAPSHOTS: usize = 32;
pub(super) const MAX_CACHED_LIST_SNAPSHOT_BYTES: usize = 64 * 1024 * 1024;
pub(super) const MAX_CACHED_LIST_SNAPSHOTS_PER_OWNER: usize = 8;
pub(super) const MAX_CACHED_LIST_SNAPSHOT_BYTES_PER_OWNER: usize = 32 * 1024 * 1024;
pub(super) const LIST_SNAPSHOT_TTL: Duration = Duration::from_secs(120);
pub(super) const LIST_CURSOR_UNAVAILABLE_MESSAGE: &str =
    "List cursor expired or is unavailable; restart listing";

pub(super) struct ListSnapshotPage {
    paths: Arc<[PathItem]>,
    range: Range<usize>,
    pub(super) next_cursor: Option<String>,
}

impl ListSnapshotPage {
    pub(super) fn from_vec(paths: Vec<PathItem>, next_cursor: Option<String>) -> Self {
        let end = paths.len();
        Self {
            paths: paths.into(),
            range: 0..end,
            next_cursor,
        }
    }

    fn from_shared(
        paths: Arc<[PathItem]>,
        range: Range<usize>,
        next_cursor: Option<String>,
    ) -> Self {
        debug_assert!(range.end <= paths.len());
        Self {
            paths,
            range,
            next_cursor,
        }
    }

    pub(super) fn paths(&self) -> &[PathItem] {
        &self.paths[self.range.clone()]
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ListSnapshotBinding {
    pub(super) owner: [u8; 32],
    pub(super) path: PathBuf,
    pub(super) directory: DirectorySnapshot,
    pub(super) sort: String,
    pub(super) order: String,
    pub(super) query: String,
    pub(super) limit: usize,
}

pub(super) struct ListSnapshotRequest<'a> {
    pub(super) owner: [u8; 32],
    pub(super) path: &'a Path,
    pub(super) directory: DirectorySnapshot,
    pub(super) sort: &'a str,
    pub(super) order: &'a str,
    pub(super) query: &'a str,
    pub(super) limit: usize,
}

impl ListSnapshotBinding {
    fn matches_request(&self, request: &ListSnapshotRequest<'_>) -> bool {
        self.owner == request.owner
            && self.path == request.path
            && self.sort == request.sort
            && self.order == request.order
            && self.query == request.query
            && self.limit == request.limit
    }
}

pub(super) struct ListSnapshotRecord {
    pub(super) secret: [u8; 32],
    pub(super) binding: ListSnapshotBinding,
    pub(super) paths: Arc<[PathItem]>,
    pub(super) expires_at: Instant,
    pub(super) weight: usize,
}

#[derive(Default)]
pub(super) struct ListSnapshotStore {
    pub(super) snapshots: HashMap<[u8; 32], ListSnapshotRecord>,
    pub(super) insertion_order: VecDeque<[u8; 32]>,
    pub(super) total_weight: usize,
}

impl ListSnapshotStore {
    fn purge_expired(&mut self, now: Instant) {
        while let Some(id) = self.insertion_order.front().copied() {
            let expired = self
                .snapshots
                .get(&id)
                .is_none_or(|snapshot| now >= snapshot.expires_at);
            if !expired {
                break;
            }
            self.remove_oldest();
        }
    }

    fn remove_oldest(&mut self) {
        while let Some(id) = self.insertion_order.pop_front() {
            if let Some(snapshot) = self.snapshots.remove(&id) {
                self.total_weight = self.total_weight.saturating_sub(snapshot.weight);
                return;
            }
        }
    }

    pub(super) fn owner_usage(&self, owner: &[u8; 32]) -> (usize, usize) {
        self.snapshots
            .values()
            .filter(|snapshot| &snapshot.binding.owner == owner)
            .fold((0usize, 0usize), |(count, weight), snapshot| {
                (
                    count.saturating_add(1),
                    weight.saturating_add(snapshot.weight),
                )
            })
    }

    fn remove_oldest_for_owner(&mut self, owner: &[u8; 32]) -> bool {
        let Some(position) = self.insertion_order.iter().position(|id| {
            self.snapshots
                .get(id)
                .is_some_and(|snapshot| &snapshot.binding.owner == owner)
        }) else {
            return false;
        };
        let Some(id) = self.insertion_order.remove(position) else {
            return false;
        };
        if let Some(snapshot) = self.snapshots.remove(&id) {
            self.total_weight = self.total_weight.saturating_sub(snapshot.weight);
            return true;
        }
        false
    }

    pub(super) fn make_room(&mut self, owner: &[u8; 32], weight: usize) {
        loop {
            let (owner_count, owner_weight) = self.owner_usage(owner);
            if owner_count < MAX_CACHED_LIST_SNAPSHOTS_PER_OWNER
                && owner_weight.saturating_add(weight) <= MAX_CACHED_LIST_SNAPSHOT_BYTES_PER_OWNER
            {
                break;
            }
            if !self.remove_oldest_for_owner(owner) {
                break;
            }
        }
        while !self.snapshots.is_empty()
            && (self.snapshots.len() >= MAX_CACHED_LIST_SNAPSHOTS
                || self.total_weight.saturating_add(weight) > MAX_CACHED_LIST_SNAPSHOT_BYTES)
        {
            self.remove_oldest();
        }
    }

    pub(super) fn page(
        &mut self,
        cursor: &DecodedListCursor,
        request: ListSnapshotRequest<'_>,
        now: Instant,
    ) -> std::result::Result<ListSnapshotPage, ListSnapshotLookupError> {
        self.purge_expired(now);
        let Some(snapshot) = self.snapshots.get(&cursor.id) else {
            return Err(ListSnapshotLookupError::Unavailable);
        };
        let expected_tag = list_cursor_tag(&snapshot.secret, &cursor.id, cursor.offset);
        {
            use subtle::ConstantTimeEq as _;

            if !bool::from(expected_tag.ct_eq(&cursor.tag)) {
                return Err(ListSnapshotLookupError::Unavailable);
            }
        }
        if !snapshot.binding.matches_request(&request) {
            return Err(ListSnapshotLookupError::InvalidBinding);
        }
        if snapshot.binding.directory != request.directory {
            return Err(ListSnapshotLookupError::DirectoryChanged);
        }

        let Ok(offset) = usize::try_from(cursor.offset) else {
            return Err(ListSnapshotLookupError::Unavailable);
        };
        if offset == 0 || offset >= snapshot.paths.len() {
            return Err(ListSnapshotLookupError::Unavailable);
        }
        let end = offset
            .saturating_add(snapshot.binding.limit)
            .min(snapshot.paths.len());
        let paths = Arc::clone(&snapshot.paths);
        let next_cursor = (end < snapshot.paths.len()).then(|| {
            encode_list_cursor(
                cursor.id,
                &snapshot.secret,
                u64::try_from(end).expect("snapshot offsets must fit in u64"),
            )
        });
        Ok(ListSnapshotPage::from_shared(
            paths,
            offset..end,
            next_cursor,
        ))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ListSnapshotLookupError {
    DirectoryChanged,
    InvalidBinding,
    Unavailable,
}

#[derive(Clone, Copy)]
pub(super) struct DecodedListCursor {
    pub(super) id: [u8; 32],
    pub(super) offset: u64,
    pub(super) tag: [u8; 16],
}

/// Owns the bounded cache used to keep immutable directory-listing pages.
///
/// Clones share the same store. A server can therefore make the cache
/// lifecycle explicit without copying snapshots, while embedders that require
/// tenant or test isolation can create a fresh instance.
#[derive(Clone, Default)]
pub(in crate::server) struct ListSnapshotCache {
    store: Arc<Mutex<ListSnapshotStore>>,
}

impl ListSnapshotCache {
    /// Creates an empty cache whose capacity is independent from every other
    /// cache instance.
    pub(in crate::server) fn isolated() -> Self {
        Self::default()
    }

    /// Returns the process-wide cache used by `ServerBuilder` unless isolation
    /// is explicitly requested.
    pub(in crate::server) fn shared_process() -> Self {
        static CACHE: OnceLock<ListSnapshotCache> = OnceLock::new();
        CACHE.get_or_init(Self::isolated).clone()
    }

    pub(super) fn cache(
        &self,
        binding: ListSnapshotBinding,
        paths: Vec<PathItem>,
        serve_root: &Path,
        now: Instant,
    ) -> ListingResult<ListSnapshotPage> {
        debug_assert!(paths.len() > binding.limit);
        let weight = list_snapshot_weight(&binding, &paths, paths.capacity());
        if weight > MAX_CACHED_LIST_SNAPSHOT_BYTES_PER_OWNER {
            return Err(ListingError::limit(
                "list_snapshot_cache",
                &binding.path,
                serve_root,
                "snapshot_memory_budget",
                ListingProblem::ListSnapshotLimit,
            ));
        }

        let first_end = binding.limit;
        let paths: Arc<[PathItem]> = paths.into();
        let mut store = self
            .store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        store.purge_expired(now);
        store.make_room(&binding.owner, weight);

        let (id, secret) = (0..8)
            .find_map(|_| {
                let mut random = [0u8; 64];
                getrandom::fill(&mut random).ok()?;
                let mut id = [0u8; 32];
                id.copy_from_slice(&random[..32]);
                if store.snapshots.contains_key(&id) {
                    return None;
                }
                let mut secret = [0u8; 32];
                secret.copy_from_slice(&random[32..]);
                Some((id, secret))
            })
            .ok_or_else(|| {
                ListingError::limit(
                    "list_snapshot_cache",
                    &binding.path,
                    serve_root,
                    "random_cursor_generation",
                    ListingProblem::DirectoryOperationFailed,
                )
            })?;

        let next_cursor = Some(encode_list_cursor(
            id,
            &secret,
            u64::try_from(first_end).expect("snapshot offsets must fit in u64"),
        ));
        store.total_weight = store.total_weight.saturating_add(weight);
        store.insertion_order.push_back(id);
        store.snapshots.insert(
            id,
            ListSnapshotRecord {
                secret,
                binding,
                paths: Arc::clone(&paths),
                expires_at: now + LIST_SNAPSHOT_TTL,
                weight,
            },
        );
        Ok(ListSnapshotPage::from_shared(
            paths,
            0..first_end,
            next_cursor,
        ))
    }

    pub(super) fn page(
        &self,
        cursor: &DecodedListCursor,
        request: ListSnapshotRequest<'_>,
        now: Instant,
    ) -> std::result::Result<ListSnapshotPage, ListSnapshotLookupError> {
        self.store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .page(cursor, request, now)
    }

    #[cfg(test)]
    pub(super) fn snapshot_count(&self) -> usize {
        self.store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .snapshots
            .len()
    }
}

pub(super) fn list_snapshot_weight(
    binding: &ListSnapshotBinding,
    paths: &[PathItem],
    path_capacity: usize,
) -> usize {
    let fixed = std::mem::size_of::<ListSnapshotRecord>()
        .saturating_add(path_capacity.saturating_mul(std::mem::size_of::<PathItem>()));
    paths.iter().fold(
        fixed
            .saturating_add(binding.path.as_os_str().as_bytes().len())
            .saturating_add(binding.sort.capacity())
            .saturating_add(binding.order.capacity())
            .saturating_add(binding.query.capacity()),
        |weight, item| {
            weight
                .saturating_add(item.name.capacity())
                .saturating_add(item.sort_name.capacity())
        },
    )
}

pub(super) fn list_snapshot_owner(account: &str) -> [u8; 32] {
    OwnerId::listing_snapshot(account).into_bytes()
}

fn list_cursor_tag(secret: &[u8; 32], id: &[u8; 32], offset: u64) -> [u8; 16] {
    let offset = offset.to_be_bytes();
    let digest = hmac_sha256(secret, &[b"dufs-list-cursor-v3\0", id, &offset]);
    let mut tag = [0u8; 16];
    tag.copy_from_slice(&digest[..16]);
    tag
}

fn hmac_sha256(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    use sha2::{Digest as _, Sha256};

    const BLOCK_BYTES: usize = 64;
    let mut key_block = [0u8; BLOCK_BYTES];
    if key.len() > BLOCK_BYTES {
        key_block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }

    let mut inner_pad = [0x36u8; BLOCK_BYTES];
    let mut outer_pad = [0x5cu8; BLOCK_BYTES];
    for index in 0..BLOCK_BYTES {
        inner_pad[index] ^= key_block[index];
        outer_pad[index] ^= key_block[index];
    }

    let mut inner = Sha256::new();
    inner.update(inner_pad);
    for part in parts {
        inner.update(part);
    }

    let mut outer = Sha256::new();
    outer.update(outer_pad);
    outer.update(inner.finalize());
    outer.finalize().into()
}

pub(super) fn encode_list_cursor(id: [u8; 32], secret: &[u8; 32], offset: u64) -> String {
    let mut bytes = [0u8; 57];
    bytes[0] = 3;
    bytes[1..33].copy_from_slice(&id);
    bytes[33..41].copy_from_slice(&offset.to_be_bytes());
    bytes[41..].copy_from_slice(&list_cursor_tag(secret, &id, offset));
    URL_SAFE_NO_PAD.encode(bytes)
}

pub(super) fn decode_list_cursor(value: &str) -> Result<DecodedListCursor> {
    if value.len() != 76 {
        return Err(anyhow!("invalid list cursor length"));
    }
    let bytes = URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|_| anyhow!("invalid list cursor encoding"))?;
    if bytes.len() != 57 || bytes[0] != 3 {
        return Err(anyhow!("unsupported list cursor version"));
    }
    if URL_SAFE_NO_PAD.encode(&bytes) != value {
        return Err(anyhow!("non-canonical list cursor encoding"));
    }
    let mut id = [0u8; 32];
    id.copy_from_slice(&bytes[1..33]);
    let mut offset_bytes = [0u8; 8];
    offset_bytes.copy_from_slice(&bytes[33..41]);
    let mut tag = [0u8; 16];
    tag.copy_from_slice(&bytes[41..]);
    Ok(DecodedListCursor {
        id,
        offset: u64::from_be_bytes(offset_bytes),
        tag,
    })
}

#[cfg(test)]
mod hmac_tests {
    use super::hmac_sha256;

    #[test]
    fn hmac_sha256_matches_rfc_4231_case_one() {
        let key = [0x0bu8; 20];
        assert_eq!(
            hmac_sha256(&key, &[b"Hi There"]),
            [
                0xb0, 0x34, 0x4c, 0x61, 0xd8, 0xdb, 0x38, 0x53, 0x5c, 0xa8, 0xaf, 0xce, 0xaf, 0x0b,
                0xf1, 0x2b, 0x88, 0x1d, 0xc2, 0x00, 0xc9, 0x83, 0x3d, 0xa7, 0x26, 0xe9, 0x37, 0x6c,
                0x2e, 0x32, 0xcf, 0xf7,
            ]
        );
    }
}
