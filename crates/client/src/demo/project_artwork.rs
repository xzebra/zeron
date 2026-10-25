//! A fabricated repository image served through the normal project-artwork RPCs.
use base64::Engine as _;
use sha2::{Digest, Sha256};

use crate::client::ClientInner;
use crate::error::{ClientError, Result};

pub(super) const ICON_PATH: &str = "apps/web/public/favicon.svg";
const SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><rect x="1" y="1" width="22" height="22" rx="6" fill="#668cf5"/><path d="M7 6h11v3h-8v3h6v3h-6v4H7Z" fill="white"/></svg>"##;

pub(super) fn rpc(
    client: &ClientInner,
    device_id: &str,
    method: &str,
    params: &serde_json::Value,
) -> Result<Option<serde_json::Value>> {
    use zeron_rpc::methods as m;
    if !matches!(method, m::READ_WORKSPACE_FILE | m::READ_WORKSPACE_IMAGE) {
        return Ok(None);
    }
    let target = params.get("target").unwrap_or(params);
    let space_id = target["spaceId"].as_str().unwrap_or_default();
    let state = client.workspace.state().0;
    if !state
        .spaces
        .iter()
        .any(|space| space.id == space_id && space.device_id == device_id)
        || params["path"].as_str() != Some(ICON_PATH)
    {
        return Err(ClientError::NotFound("Demo project image".into()));
    }
    let checkout_id = format!("demo-{space_id}");
    let content_hash = format!("{:x}", Sha256::digest(SVG.as_bytes()));
    if method == m::READ_WORKSPACE_FILE {
        return Ok(Some(serde_json::json!({
            "checkoutId": checkout_id, "path": ICON_PATH, "text": SVG,
            "contentHash": content_hash, "size": SVG.len(),
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
        || request.offset >= SVG.len()
    {
        return Err(ClientError::InvalidArgument(
            "Demo image identity or offset changed".into(),
        ));
    }
    let end = (request.offset + zeron_proto::WORKSPACE_IMAGE_CHUNK_BYTES).min(SVG.len());
    Ok(Some(serde_json::json!({
        "checkoutId": checkout_id, "contentHash": content_hash,
        "mimeType": "image/svg+xml",
        "data": base64::engine::general_purpose::STANDARD.encode(&SVG.as_bytes()[request.offset..end]),
        "nextOffset": end, "size": SVG.len(), "done": end == SVG.len(),
    })))
}
