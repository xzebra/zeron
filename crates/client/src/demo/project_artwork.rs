//! A fabricated repository image served through the normal project-artwork RPCs.
use base64::Engine as _;
use sha2::{Digest, Sha256};

use crate::client::ClientInner;
use crate::error::{ClientError, Result};

pub(super) const ICON_PATH: &str = "apps/web/public/favicon.svg";
const SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><rect x="1" y="1" width="22" height="22" rx="6" fill="#668cf5"/><path d="M7 6h11v3h-8v3h6v3h-6v4H7Z" fill="white"/></svg>"##;
const IMAGES: &[(&str, &str)] = &[
    ("public/favicon.svg", "#94a3b8"),
    (ICON_PATH, "#668cf5"),
    ("assets/brand/logo.svg", "#ff5a1f"),
    ("assets/icons/app-icon.svg", "#22c55e"),
    ("design/mark.svg", "#a855f7"),
];

pub(super) fn rpc(
    client: &ClientInner,
    device_id: &str,
    method: &str,
    params: &serde_json::Value,
) -> Result<Option<serde_json::Value>> {
    use zeron_rpc::methods as m;
    if !matches!(
        method,
        m::READ_WORKSPACE_FILE | m::READ_WORKSPACE_IMAGE | m::SEARCH_WORKSPACE_FILES
    ) {
        return Ok(None);
    }
    let target = params;
    let space_id = target["spaceId"].as_str().unwrap_or_default();
    let state = client.workspace.state().0;
    if !state
        .spaces
        .iter()
        .any(|space| space.id == space_id && space.device_id == device_id)
    {
        return Err(ClientError::NotFound("Demo project image".into()));
    }
    if method == m::SEARCH_WORKSPACE_FILES {
        let request: zeron_proto::SearchWorkspaceFilesRequest =
            serde_json::from_value(params.clone())
                .map_err(|_| ClientError::InvalidArgument("Invalid demo image search".into()))?;
        let query = request
            .query
            .split_whitespace()
            .next()
            .filter(|word| !word.contains('*'))
            .unwrap_or_default()
            .to_lowercase();
        let results: Vec<_> = IMAGES
            .iter()
            .filter(|(path, _)| query.is_empty() || path.to_lowercase().contains(&query))
            .map(|(path, _)| {
                serde_json::json!({
                    "path": path, "name": path.rsplit('/').next().unwrap_or(path),
                    "kind": "file", "score": 1,
                })
            })
            .collect();
        return Ok(Some(serde_json::json!(results)));
    }
    let path = params["path"].as_str().unwrap_or_default();
    let (_, color) = IMAGES
        .iter()
        .find(|(candidate, _)| *candidate == path)
        .ok_or_else(|| ClientError::NotFound("Demo project image".into()))?;
    let svg = SVG.replace("#668cf5", color);
    let checkout_id = format!("demo-{space_id}");
    let content_hash = format!("{:x}", Sha256::digest(svg.as_bytes()));
    if method == m::READ_WORKSPACE_FILE {
        let _: zeron_proto::ReadWorkspaceFileRequest = serde_json::from_value(params.clone())
            .map_err(|_| ClientError::InvalidArgument("Invalid demo image request".into()))?;
        return Ok(Some(serde_json::json!({
            "checkoutId": checkout_id, "path": path, "text": svg,
            "contentHash": content_hash, "size": svg.len(),
            "encoding": "utf8", "truncated": false,
        })));
    }
    let request: zeron_proto::ReadWorkspaceImageRequest = serde_json::from_value(params.clone())
        .map_err(|_| ClientError::InvalidArgument("Invalid demo image request".into()))?;
    if request.expected_checkout_id != checkout_id
        || request
            .expected_content_hash
            .as_ref()
            .is_some_and(|hash| hash != &content_hash)
        || request.offset >= svg.len()
    {
        return Err(ClientError::InvalidArgument(
            "Demo image identity or offset changed".into(),
        ));
    }
    let end = (request.offset + zeron_proto::WORKSPACE_IMAGE_CHUNK_BYTES).min(svg.len());
    Ok(Some(serde_json::json!({
        "checkoutId": checkout_id, "contentHash": content_hash,
        "mimeType": "image/svg+xml",
        "data": base64::engine::general_purpose::STANDARD.encode(&svg.as_bytes()[request.offset..end]),
        "nextOffset": end, "size": svg.len(), "done": end == svg.len(),
    })))
}
