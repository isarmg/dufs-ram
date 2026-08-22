use super::{Response, Server, status_not_found};
use crate::http_utils::body_full;

use crate::utils::encode_hex;
use bytes::Bytes;
use headers::{ContentLength, HeaderMapExt};
use hyper::{
    StatusCode,
    header::{CACHE_CONTROL, HeaderValue},
};
use sha2::{Digest, Sha256};

struct EmbeddedAsset {
    name: &'static str,
    contents: &'static [u8],
    content_type: &'static str,
}

const EMBEDDED_ASSETS: &[EmbeddedAsset] = &[
    EmbeddedAsset {
        name: "index.js",
        contents: include_str!("../../assets/index.js").as_bytes(),
        content_type: "application/javascript; charset=UTF-8",
    },
    EmbeddedAsset {
        name: "index.css",
        contents: include_str!("../../assets/index.css").as_bytes(),
        content_type: "text/css; charset=UTF-8",
    },
    EmbeddedAsset {
        name: "login.css",
        contents: include_str!("../../assets/login.css").as_bytes(),
        content_type: "text/css; charset=UTF-8",
    },
    EmbeddedAsset {
        name: "favicon.ico",
        contents: include_bytes!("../../assets/favicon.ico"),
        content_type: "image/x-icon",
    },
    EmbeddedAsset {
        name: "modules/app.js",
        contents: include_str!("../../assets/modules/app.js").as_bytes(),
        content_type: "application/javascript; charset=UTF-8",
    },
    EmbeddedAsset {
        name: "modules/http/client.js",
        contents: include_str!("../../assets/modules/http/client.js").as_bytes(),
        content_type: "application/javascript; charset=UTF-8",
    },
    EmbeddedAsset {
        name: "modules/http/headers.js",
        contents: include_str!("../../assets/modules/http/headers.js").as_bytes(),
        content_type: "application/javascript; charset=UTF-8",
    },
    EmbeddedAsset {
        name: "modules/http/response_buffer.js",
        contents: include_str!("../../assets/modules/http/response_buffer.js").as_bytes(),
        content_type: "application/javascript; charset=UTF-8",
    },
    EmbeddedAsset {
        name: "modules/listing/controller.js",
        contents: include_str!("../../assets/modules/listing/controller.js").as_bytes(),
        content_type: "application/javascript; charset=UTF-8",
    },
    EmbeddedAsset {
        name: "modules/operations/dialogs.js",
        contents: include_str!("../../assets/modules/operations/dialogs.js").as_bytes(),
        content_type: "application/javascript; charset=UTF-8",
    },
    EmbeddedAsset {
        name: "modules/operations/file_operations.js",
        contents: include_str!("../../assets/modules/operations/file_operations.js").as_bytes(),
        content_type: "application/javascript; charset=UTF-8",
    },
    EmbeddedAsset {
        name: "modules/shared/dom.js",
        contents: include_str!("../../assets/modules/shared/dom.js").as_bytes(),
        content_type: "application/javascript; charset=UTF-8",
    },
    EmbeddedAsset {
        name: "modules/shared/index_data.js",
        contents: include_str!("../../assets/modules/shared/index_data.js").as_bytes(),
        content_type: "application/javascript; charset=UTF-8",
    },
    EmbeddedAsset {
        name: "modules/shared/mutation_effect.js",
        contents: include_str!("../../assets/modules/shared/mutation_effect.js").as_bytes(),
        content_type: "application/javascript; charset=UTF-8",
    },
    EmbeddedAsset {
        name: "modules/shared/path.js",
        contents: include_str!("../../assets/modules/shared/path.js").as_bytes(),
        content_type: "application/javascript; charset=UTF-8",
    },
    EmbeddedAsset {
        name: "modules/upload/manager.js",
        contents: include_str!("../../assets/modules/upload/manager.js").as_bytes(),
        content_type: "application/javascript; charset=UTF-8",
    },
    EmbeddedAsset {
        name: "modules/upload/preflight.js",
        contents: include_str!("../../assets/modules/upload/preflight.js").as_bytes(),
        content_type: "application/javascript; charset=UTF-8",
    },
    EmbeddedAsset {
        name: "modules/upload/protocol.js",
        contents: include_str!("../../assets/modules/upload/protocol.js").as_bytes(),
        content_type: "application/javascript; charset=UTF-8",
    },
    EmbeddedAsset {
        name: "modules/upload/queue.js",
        contents: include_str!("../../assets/modules/upload/queue.js").as_bytes(),
        content_type: "application/javascript; charset=UTF-8",
    },
    EmbeddedAsset {
        name: "modules/upload/selection.js",
        contents: include_str!("../../assets/modules/upload/selection.js").as_bytes(),
        content_type: "application/javascript; charset=UTF-8",
    },
    EmbeddedAsset {
        name: "modules/upload/transport.js",
        contents: include_str!("../../assets/modules/upload/transport.js").as_bytes(),
        content_type: "application/javascript; charset=UTF-8",
    },
    EmbeddedAsset {
        name: "modules/upload/view.js",
        contents: include_str!("../../assets/modules/upload/view.js").as_bytes(),
        content_type: "application/javascript; charset=UTF-8",
    },
];

pub(super) fn embedded_assets_prefix() -> String {
    let mut digest = Sha256::new();
    for asset in EMBEDDED_ASSETS {
        digest.update((asset.name.len() as u64).to_be_bytes());
        digest.update(asset.name.as_bytes());
        digest.update((asset.content_type.len() as u64).to_be_bytes());
        digest.update(asset.content_type.as_bytes());
        digest.update((asset.contents.len() as u64).to_be_bytes());
        digest.update(asset.contents);
    }
    format!("__dufs_assets_{}/", encode_hex(digest.finalize()))
}

impl Server {
    pub(super) fn handle_internal(
        &self,
        req_path: &str,
        head_only: bool,
        res: &mut Response,
    ) -> bool {
        let Some(name) = req_path.strip_prefix(&self.content.assets_prefix) else {
            return false;
        };
        let Some(asset) = embedded_asset(name) else {
            status_not_found(res);
            res.headers_mut().insert(
                "x-content-type-options",
                HeaderValue::from_static("nosniff"),
            );
            return true;
        };

        res.headers_mut()
            .typed_insert(ContentLength(asset.contents.len() as u64));
        if !head_only {
            *res.body_mut() = body_full(Bytes::from_static(asset.contents));
        }
        res.headers_mut()
            .insert("content-type", HeaderValue::from_static(asset.content_type));
        debug_assert_eq!(res.status(), StatusCode::OK);
        res.headers_mut().insert(
            CACHE_CONTROL,
            HeaderValue::from_static("public, max-age=31536000, immutable"),
        );
        res.headers_mut().insert(
            "x-content-type-options",
            HeaderValue::from_static("nosniff"),
        );
        true
    }

    pub(super) fn is_public_asset_path(&self, req_path: &str) -> bool {
        req_path
            .strip_prefix(&self.content.assets_prefix)
            .is_some_and(|name| embedded_asset(name).is_some())
    }
}

fn embedded_asset(name: &str) -> Option<&'static EmbeddedAsset> {
    EMBEDDED_ASSETS.iter().find(|asset| asset.name == name)
}
