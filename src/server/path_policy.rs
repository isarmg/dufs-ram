use super::internal_names::is_internal_name;
use crate::utils::{decode_uri, encode_uri};

use std::{
    ops::Deref,
    path::{Component, Path, PathBuf},
};

pub(super) const BROWSER_COMPONENT_BYTES_LIMIT: usize = 255;
pub(super) const BROWSER_RELATIVE_PATH_BYTES_LIMIT: usize = 4095;

/// Normalized path used for HTTP route selection. Construction is the only
/// place where percent decoding and reserved-namespace canonicalization occur.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct RoutePath(String);

impl RoutePath {
    pub(super) fn as_str(&self) -> &str {
        &self.0
    }
}

impl Deref for RoutePath {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.as_str()
    }
}

/// A lexical path proven to be rooted beneath the configured serve directory.
/// Filesystem containment is still enforced independently by `RootedFs`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct RootedPath(PathBuf);

impl RootedPath {
    pub(super) fn as_path(&self) -> &Path {
        &self.0
    }

    pub(super) fn into_path_buf(self) -> PathBuf {
        self.0
    }
}

impl AsRef<Path> for RootedPath {
    fn as_ref(&self) -> &Path {
        self.as_path()
    }
}

/// Owns every lexical path rule shared by the transport router and browser
/// API. It intentionally does not replace `RootedFs`, which supplies the
/// kernel-enforced symlink/containment boundary.
#[derive(Clone, Debug)]
pub(super) struct PathPolicy {
    root: PathBuf,
    assets_component: String,
}

impl PathPolicy {
    pub(super) fn new(root: PathBuf, assets_prefix: &str) -> Self {
        let assets_component = assets_prefix
            .strip_suffix('/')
            .unwrap_or(assets_prefix)
            .to_owned();
        Self {
            root,
            assets_component,
        }
    }

    pub(super) fn parse_route(&self, raw_path: &str) -> Option<RoutePath> {
        let path = decode_uri(raw_path)?;
        if path.as_bytes().contains(&0) {
            return None;
        }

        let mut parts = Vec::new();
        for component in Path::new(path.trim_matches('/')).components() {
            let Component::Normal(value) = component else {
                return None;
            };
            let value = value.to_string_lossy();
            if value.len() > BROWSER_COMPONENT_BYTES_LIMIT || is_internal_name(&value) {
                return None;
            }
            parts.push(value);
        }

        let normalized = parts.join("/");
        if normalized.len() > BROWSER_RELATIVE_PATH_BYTES_LIMIT {
            return None;
        }
        if normalized
            .split('/')
            .next()
            .is_some_and(|component| self.is_reserved_component(component))
            && raw_path != format!("/{}", encode_uri(&normalized))
        {
            return None;
        }
        Some(RoutePath(normalized))
    }

    /// Parse the absolute, slash-prefixed paths accepted by browser mutation
    /// JSON. Empty segments and dot components are rejected instead of being
    /// silently normalized because these values identify mutation targets.
    pub(super) fn parse_browser_target(&self, value: &str) -> Option<RootedPath> {
        let relative = value.strip_prefix('/')?;
        if relative.is_empty()
            || relative.len() > BROWSER_RELATIVE_PATH_BYTES_LIMIT
            || relative.contains('\0')
        {
            return None;
        }

        let mut resolved = self.root.clone();
        for (index, component) in relative.split('/').enumerate() {
            if component.is_empty()
                || component == "."
                || component == ".."
                || component.len() > BROWSER_COMPONENT_BYTES_LIMIT
                || is_internal_name(component)
                || (index == 0 && self.is_reserved_component(component))
            {
                return None;
            }
            resolved.push(component);
        }
        Some(RootedPath(resolved))
    }

    /// Parse the logical path accepted by the paginated listing API. Unlike
    /// mutation targets, the shared root itself is a valid list target; all
    /// non-root paths use the exact same lexical and reserved-name policy as
    /// browser mutations.
    pub(super) fn parse_list_target(&self, value: &str) -> Option<RootedPath> {
        if value == "/" {
            Some(RootedPath(self.root.clone()))
        } else {
            self.parse_browser_target(value)
        }
    }

    pub(super) fn resolve_route(&self, path: &RoutePath) -> RootedPath {
        if path.as_str().is_empty() {
            RootedPath(self.root.clone())
        } else {
            RootedPath(self.root.join(path.as_str()))
        }
    }

    pub(super) fn is_managed_root(&self, path: &Path) -> bool {
        path == self.root
    }

    pub(super) fn is_reserved_component(&self, component: &str) -> bool {
        component == "__dufs__" || component == self.assets_component
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> PathPolicy {
        PathPolicy::new(PathBuf::from("/srv/share"), "__dufs_assets_test/")
    }

    #[test]
    fn route_paths_are_normalized_once() {
        let policy = policy();
        assert_eq!(policy.parse_route("/a%20b/c").unwrap().as_str(), "a b/c");
        assert_eq!(
            policy.parse_route("//ordinary//path/").unwrap().as_str(),
            "ordinary/path"
        );
        assert!(policy.parse_route("/%00").is_none());
        assert!(policy.parse_route("/../escape").is_none());
    }

    #[test]
    fn internal_routes_require_their_canonical_encoding() {
        let policy = policy();
        assert!(policy.parse_route("/__dufs__/health").is_some());
        assert!(policy.parse_route("//__dufs__/health").is_none());
        assert!(policy.parse_route("/__dufs__//health").is_none());
        assert!(policy.parse_route("/__dufs_assets_test/index.js").is_some());
    }

    #[test]
    fn browser_targets_are_non_root_and_exclude_internal_namespaces() {
        let policy = policy();
        assert_eq!(
            policy
                .parse_browser_target("/folder/file.txt")
                .unwrap()
                .as_path(),
            Path::new("/srv/share/folder/file.txt")
        );
        for invalid in [
            "/",
            "relative",
            "/folder//file",
            "/folder/../file",
            "/__dufs__/health",
            "/__dufs_assets_test/index.js",
        ] {
            assert!(policy.parse_browser_target(invalid).is_none(), "{invalid}");
        }
    }

    #[test]
    fn list_targets_share_mutation_rules_but_allow_the_root() {
        let policy = policy();
        assert_eq!(
            policy.parse_list_target("/").unwrap().as_path(),
            Path::new("/srv/share")
        );
        assert_eq!(
            policy
                .parse_list_target("/folder/file.txt")
                .unwrap()
                .as_path(),
            Path::new("/srv/share/folder/file.txt")
        );
        for invalid in [
            "relative",
            "/folder//file",
            "/folder/../file",
            "/__dufs__/health",
            "/__dufs_assets_test/index.js",
        ] {
            assert!(policy.parse_list_target(invalid).is_none(), "{invalid}");
        }
    }

    #[test]
    fn rooted_paths_fit_linux_openat_pathname_limits() {
        let policy = policy();
        let longest_component = "a".repeat(BROWSER_COMPONENT_BYTES_LIMIT);
        assert!(
            policy
                .parse_browser_target(&format!("/{longest_component}"))
                .is_some()
        );
        assert!(
            policy
                .parse_browser_target(&format!(
                    "/{}",
                    "a".repeat(BROWSER_COMPONENT_BYTES_LIMIT + 1)
                ))
                .is_none()
        );

        let longest_relative = std::iter::repeat_n(longest_component.as_str(), 16)
            .collect::<Vec<_>>()
            .join("/");
        assert_eq!(longest_relative.len(), BROWSER_RELATIVE_PATH_BYTES_LIMIT);
        assert!(
            policy
                .parse_browser_target(&format!("/{longest_relative}"))
                .is_some()
        );

        let overlong_relative = [
            std::iter::repeat_n(longest_component.as_str(), 15)
                .collect::<Vec<_>>()
                .join("/"),
            "b".repeat(254),
            "c".to_string(),
        ]
        .join("/");
        assert_eq!(
            overlong_relative.len(),
            BROWSER_RELATIVE_PATH_BYTES_LIMIT + 1
        );
        assert!(
            policy
                .parse_browser_target(&format!("/{overlong_relative}"))
                .is_none()
        );
        assert!(
            policy
                .parse_route(&format!("/{overlong_relative}"))
                .is_none()
        );
    }
}
