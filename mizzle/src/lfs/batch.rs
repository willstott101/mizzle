//! Git LFS batch API handler.
//!
//! Parses a `BatchRequest`, authorises the operation, and returns a JSON
//! `BatchResponse` with per-object transfer actions.

use std::collections::HashMap;
use std::time::SystemTime;

use mizzle_proto::lfs::{
    BatchActionDetail, BatchObjectActions, BatchObjectError, BatchRef, BatchRequest, BatchResponse,
    BatchResponseObject, Operation,
};

use tracing::{debug, error, info, warn};

use super::{LfsOid, LfsStore, TransferAction};
use crate::traits::RepoAccess;

/// Handle a Git LFS batch request.
///
/// Returns `(http_status, json_body)`.  The JSON body is always valid JSON
/// (even for error responses it is an LFS error object).
pub async fn handle_batch<A, L>(
    access: &A,
    lfs: &L,
    repo_id: &A::RepoId,
    lfs_base_url: &str,
    body: &[u8],
) -> (u16, String)
where
    A: RepoAccess,
    L: LfsStore<RepoId = A::RepoId>,
{
    // 1. Parse BatchRequest.
    let req: BatchRequest = match serde_json::from_slice(body) {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, "LFS batch: invalid request body");
            return (
                400,
                serde_json::json!({"message": format!("invalid request: {e}")}).to_string(),
            );
        }
    };

    let op = req.operation;
    let object_count = req.objects.len();
    let git_ref_name = req.git_ref.as_ref().map(|r| r.name.as_str()).unwrap_or("");
    debug!(
        operation = ?op,
        object_count,
        git_ref = git_ref_name,
        "LFS batch request"
    );

    // 2. Authorise.
    let git_ref: Option<String> = req.git_ref.as_ref().map(|r| r.name.clone());
    if let Err(reason) = access.authorize_lfs(req.operation, git_ref.as_deref()) {
        warn!(operation = ?op, reason, "LFS batch: authorization denied");
        return (403, serde_json::json!({"message": reason}).to_string());
    }

    // 3. Open the LFS store.
    let repo = match lfs.open(repo_id).await {
        Ok(r) => r,
        Err(e) => {
            error!(error = %e, "LFS batch: failed to open LFS store");
            return (
                500,
                serde_json::json!({"message": format!("failed to open LFS store: {e}")})
                    .to_string(),
            );
        }
    };

    // 4. Build per-object responses.
    let mut objects: Vec<BatchResponseObject> = Vec::with_capacity(req.objects.len());

    for obj in &req.objects {
        let stat = match lfs.stat(&repo, &obj.oid).await {
            Ok(s) => s,
            Err(e) => {
                objects.push(BatchResponseObject {
                    oid: obj.oid,
                    size: obj.size,
                    actions: None,
                    error: Some(BatchObjectError {
                        code: 500,
                        message: format!("stat error: {e}"),
                    }),
                });
                continue;
            }
        };

        match req.operation {
            Operation::Download => {
                if stat.is_none() {
                    // Object not present — per-object 404.
                    objects.push(BatchResponseObject {
                        oid: obj.oid,
                        size: obj.size,
                        actions: None,
                        error: Some(BatchObjectError {
                            code: 404,
                            message: "object not found".to_string(),
                        }),
                    });
                } else {
                    let size = stat.unwrap();
                    match lfs.download_action(&repo, &obj.oid, size).await {
                        Ok(action) => {
                            let detail = transfer_action_to_detail(
                                action,
                                &obj.oid,
                                size,
                                lfs_base_url,
                                false,
                            );
                            objects.push(BatchResponseObject {
                                oid: obj.oid,
                                size: obj.size,
                                actions: Some(BatchObjectActions {
                                    download: Some(detail),
                                    upload: None,
                                    verify: None,
                                }),
                                error: None,
                            });
                        }
                        Err(e) => {
                            objects.push(BatchResponseObject {
                                oid: obj.oid,
                                size: obj.size,
                                actions: None,
                                error: Some(BatchObjectError {
                                    code: 500,
                                    message: format!("download_action error: {e}"),
                                }),
                            });
                        }
                    }
                }
            }
            Operation::Upload => {
                if stat.is_some() {
                    // Already present — no action needed.
                    objects.push(BatchResponseObject {
                        oid: obj.oid,
                        size: obj.size,
                        actions: None,
                        error: None,
                    });
                } else {
                    match lfs.upload_action(&repo, &obj.oid, obj.size).await {
                        Ok(action) => {
                            let upload_detail = transfer_action_to_detail(
                                action,
                                &obj.oid,
                                obj.size,
                                lfs_base_url,
                                false,
                            );
                            let verify_detail = BatchActionDetail {
                                href: format!("{lfs_base_url}/objects/verify"),
                                header: HashMap::new(),
                                expires_in: None,
                            };
                            objects.push(BatchResponseObject {
                                oid: obj.oid,
                                size: obj.size,
                                actions: Some(BatchObjectActions {
                                    download: None,
                                    upload: Some(upload_detail),
                                    verify: Some(verify_detail),
                                }),
                                error: None,
                            });
                        }
                        Err(e) => {
                            objects.push(BatchResponseObject {
                                oid: obj.oid,
                                size: obj.size,
                                actions: None,
                                error: Some(BatchObjectError {
                                    code: 500,
                                    message: format!("upload_action error: {e}"),
                                }),
                            });
                        }
                    }
                }
            }
        }
    }

    let response = BatchResponse {
        transfer: "basic".to_string(),
        objects,
    };

    match serde_json::to_string(&response) {
        Ok(json) => {
            info!(
                operation = ?op,
                object_count,
                "LFS batch: response ready"
            );
            (200, json)
        }
        Err(e) => {
            error!(error = %e, "LFS batch: serialization error");
            (
                500,
                serde_json::json!({"message": format!("serialization error: {e}")}).to_string(),
            )
        }
    }
}

/// Convert a [`TransferAction`] into a [`BatchActionDetail`] for the batch response.
///
/// For `Proxy`, synthesises the `href` from `lfs_base_url` and `oid`.
/// For `Redirect`, passes `href`/`header`/`expires_at` through directly.
fn transfer_action_to_detail(
    action: TransferAction,
    oid: &LfsOid,
    _size: u64,
    lfs_base_url: &str,
    _is_upload: bool,
) -> BatchActionDetail {
    match action {
        TransferAction::Proxy => BatchActionDetail {
            href: format!("{lfs_base_url}/objects/{}", oid.to_hex()),
            header: HashMap::new(),
            expires_in: None,
        },
        TransferAction::Redirect {
            href,
            header,
            expires_at,
        } => {
            let expires_in = expires_at.and_then(|t| {
                t.duration_since(SystemTime::now())
                    .ok()
                    .map(|d| d.as_secs())
            });
            BatchActionDetail {
                href,
                header: header.into_iter().collect(),
                expires_in,
            }
        }
    }
}

/// Parse a [`BatchRef`] from an optional name string (for convenience).
#[allow(dead_code)]
pub fn make_batch_ref(name: impl Into<String>) -> BatchRef {
    BatchRef { name: name.into() }
}
