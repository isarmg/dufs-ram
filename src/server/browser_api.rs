use super::{
    Request, Response, Server, rooted_fs::RootedFs, status_bad_request, status_error,
    status_no_content, status_not_found, upload::is_upload_temp_name,
};

use anyhow::Result;
use http_body_util::{BodyExt, LengthLimitError, Limited};
use hyper::{StatusCode, header::CONTENT_TYPE};
use serde::Deserialize;
use std::path::{Path, PathBuf};

pub(super) const BROWSER_API_PREFIX: &str = "__dufs__/api/";
const MKDIR_API_PATH: &str = "__dufs__/api/mkdir";
const MOVE_API_PATH: &str = "__dufs__/api/move";
const API_BODY_LIMIT: usize = 16 * 1024;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MkdirRequest {
    path: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MoveRequest {
    source: String,
    destination: String,
    #[serde(default)]
    overwrite: bool,
}

impl Server {
    pub(super) async fn handle_browser_api(
        &self,
        endpoint: &str,
        req: Request,
        res: &mut Response,
    ) -> Result<()> {
        if endpoint != MKDIR_API_PATH && endpoint != MOVE_API_PATH {
            status_not_found(res);
            return Ok(());
        }
        let is_json = req
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/json"));
        if !is_json {
            status_error(
                res,
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "Content-Type must be application/json",
            );
            return Ok(());
        }

        let body = match Limited::new(req.into_body(), API_BODY_LIMIT)
            .collect()
            .await
        {
            Ok(body) => body.to_bytes(),
            Err(err) => {
                if err.downcast_ref::<LengthLimitError>().is_some() {
                    status_error(res, StatusCode::PAYLOAD_TOO_LARGE, "Request body too large");
                } else {
                    status_bad_request(res, "Invalid request body");
                }
                return Ok(());
            }
        };

        match endpoint {
            MKDIR_API_PATH => match serde_json::from_slice::<MkdirRequest>(&body) {
                Ok(request) => self.handle_api_mkdir(request, res).await?,
                Err(_) => status_bad_request(res, "Invalid JSON request"),
            },
            MOVE_API_PATH => match serde_json::from_slice::<MoveRequest>(&body) {
                Ok(request) => self.handle_api_move(request, res).await?,
                Err(_) => status_bad_request(res, "Invalid JSON request"),
            },
            _ => unreachable!(),
        }
        Ok(())
    }

    async fn handle_api_mkdir(&self, request: MkdirRequest, res: &mut Response) -> Result<()> {
        let path = match self.resolve_browser_path(&request.path) {
            Some(path) => path,
            None => {
                status_bad_request(res, "Invalid path");
                return Ok(());
            }
        };
        let path_lease = self.path_coordinator.acquire([&path]).await;

        if self.guard_root_contained(&path).await {
            status_bad_request(res, "Invalid path");
            return Ok(());
        }
        match self.rooted_fs.metadata_nofollow(&path).await {
            Ok(_) => {
                status_error(res, StatusCode::CONFLICT, "Path already exists");
                return Ok(());
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        }

        let rooted_fs = self.rooted_fs.clone();
        let commit_path = path.clone();
        let create_result = self
            .run_commit(async move {
                let _path_lease = path_lease;
                match rooted_fs.create_directory(&commit_path).await {
                    Ok(created_ancestors) => Ok(Ok(created_ancestors)),
                    Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => Ok(Err(err)),
                    Err(err) => Err(err.into()),
                }
            })
            .await?;
        match create_result {
            Ok(_) => {}
            Err(_) => {
                status_error(res, StatusCode::CONFLICT, "Path already exists");
                return Ok(());
            }
        }
        *res.status_mut() = StatusCode::CREATED;
        Ok(())
    }

    async fn handle_api_move(&self, request: MoveRequest, res: &mut Response) -> Result<()> {
        let source = match self.resolve_browser_path(&request.source) {
            Some(path) => path,
            None => {
                status_bad_request(res, "Invalid source path");
                return Ok(());
            }
        };
        let destination = match self.resolve_browser_path(&request.destination) {
            Some(path) => path,
            None => {
                status_bad_request(res, "Invalid destination path");
                return Ok(());
            }
        };
        if source == destination {
            status_bad_request(res, "Source and destination must differ");
            return Ok(());
        }
        let path_lease = self.path_coordinator.acquire([&source, &destination]).await;

        if self.guard_root_contained(&source).await || self.guard_root_contained(&destination).await
        {
            status_bad_request(res, "Invalid move path");
            return Ok(());
        }
        let source_meta = match self.rooted_fs.metadata_nofollow(&source).await {
            Ok(meta) => meta,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                status_not_found(res);
                return Ok(());
            }
            Err(err) => return Err(err.into()),
        };
        if source_meta.is_dir() && destination.starts_with(&source) {
            status_error(
                res,
                StatusCode::CONFLICT,
                "A directory cannot be moved into itself",
            );
            return Ok(());
        }
        let destination_meta = match self.rooted_fs.metadata_nofollow(&destination).await {
            Ok(meta) => Some(meta),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            Err(err) => return Err(err.into()),
        };
        if let Some(destination_meta) = &destination_meta {
            if !request.overwrite {
                status_error(res, StatusCode::CONFLICT, "Destination already exists");
                return Ok(());
            }
            if source_meta.is_dir() || destination_meta.is_dir() {
                status_error(
                    res,
                    StatusCode::CONFLICT,
                    "Directories cannot be overwritten",
                );
                return Ok(());
            }
        }

        if self.guard_root_contained(&destination).await {
            status_bad_request(res, "Invalid destination path");
            return Ok(());
        }

        let rooted_fs = self.rooted_fs.clone();
        let commit_source = source.clone();
        let commit_destination = destination.clone();
        let overwrite = request.overwrite;
        let moved = self
            .run_commit(async move {
                let _path_lease = path_lease;
                commit_move(&rooted_fs, &commit_source, &commit_destination, overwrite).await
            })
            .await?;
        match moved {
            true => {}
            false => {
                status_error(res, StatusCode::CONFLICT, "Destination already exists");
                return Ok(());
            }
        }

        status_no_content(res);
        Ok(())
    }

    fn resolve_browser_path(&self, path: &str) -> Option<PathBuf> {
        let relative = path.strip_prefix('/')?;
        if relative.contains('\0') {
            return None;
        }

        let mut resolved = self.args.serve_path.clone();
        if !relative.is_empty() {
            for part in relative.split('/') {
                if part.is_empty() || part == "." || part == ".." || is_upload_temp_name(part) {
                    return None;
                }
                resolved.push(part);
            }
        }

        if self.is_managed_root(&resolved) {
            return None;
        }

        if resolved
            .strip_prefix(&self.args.serve_path)
            .ok()?
            .components()
            .next()
            .and_then(|part| part.as_os_str().to_str())
            .is_some_and(|part| self.is_reserved_internal_component(part))
        {
            return None;
        }
        Some(resolved)
    }
}

async fn commit_move(
    rooted_fs: &RootedFs,
    source: &Path,
    destination: &Path,
    overwrite: bool,
) -> Result<bool> {
    if overwrite {
        rooted_fs.rename_replace(source, destination).await?;
        return Ok(true);
    }
    Ok(rooted_fs.rename_no_replace(source, destination).await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn move_commit_rejects_target_created_after_precheck() {
        let temp = assert_fs::TempDir::new().unwrap();
        let source = temp.path().join("source.txt");
        let destination = temp.path().join("destination.txt");
        let rooted_fs = RootedFs::new(temp.path()).unwrap();
        std::fs::write(&source, "source-content").unwrap();

        std::fs::write(&destination, "competitor-content").unwrap();
        assert!(
            !commit_move(&rooted_fs, &source, &destination, false)
                .await
                .unwrap()
        );
        assert_eq!(std::fs::read_to_string(&source).unwrap(), "source-content");
        assert_eq!(
            std::fs::read_to_string(&destination).unwrap(),
            "competitor-content"
        );
    }
}
