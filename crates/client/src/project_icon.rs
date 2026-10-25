//! Project artwork fetched from its owning host; never persisted as a local file.
use crate::error::Result;
use crate::{Client, ClientError};
use base64::Engine as _;
use zeron_proto::{MAX_WORKSPACE_IMAGE_BYTES, WORKSPACE_IMAGE_CHUNK_BYTES, WorkspaceTarget};

impl Client {
    pub async fn project_icon(&self, space_id: &str) -> Result<(String, Vec<u8>)> {
        let state = self.inner.workspace.state().0;
        let spaces = &state.spaces;
        let space = spaces
            .iter()
            .find(|s| s.id == space_id)
            .ok_or_else(|| ClientError::NotFound("Project".into()))?;
        let representative = zeron_proto::view::representative_space(spaces, space);
        let icon_path =
            zeron_proto::project_icon::normalize_path(representative.icon_path.as_deref())
                .map_err(|s| ClientError::InvalidArgument(s.into()))?;
        let target = WorkspaceTarget {
            chat_id: None,
            space_id: Some(space.id.clone()),
            checkout_path: None,
        };
        for path in zeron_proto::project_icon::candidates(icon_path.as_deref()) {
            let response = self
                .inner
                .host_rpc(
                    &space.device_id,
                    crate::rpc::methods::READ_WORKSPACE_FILE,
                    serde_json::json!({"target": target, "path": path}),
                )
                .await;
            let file: zeron_proto::WorkspaceFileText = match response {
                Ok(value) => serde_json::from_value(value)
                    .map_err(|_| ClientError::Internal("Invalid workspace response".into()))?,
                Err(ClientError::HostUnavailable(message)) => {
                    return Err(ClientError::HostUnavailable(message));
                }
                Err(_) if icon_path.is_none() => continue,
                Err(error) => return Err(error),
            };
            return self
                .project_icon_image(&space.device_id, target, path.into(), file.checkout_id)
                .await;
        }
        Err(ClientError::NotFound("Project artwork".into()))
    }

    async fn project_icon_image(
        &self,
        device_id: &str,
        target: WorkspaceTarget,
        path: String,
        checkout_id: String,
    ) -> Result<(String, Vec<u8>)> {
        let mut request = zeron_proto::ReadWorkspaceImageRequest {
            target,
            path,
            expected_checkout_id: checkout_id,
            offset: 0,
            expected_content_hash: None,
        };
        let mut bytes = Vec::new();
        let mut mime = None;
        let mut size = None;
        for _ in 0..=MAX_WORKSPACE_IMAGE_BYTES / WORKSPACE_IMAGE_CHUNK_BYTES {
            let chunk: zeron_proto::WorkspaceImageChunk = serde_json::from_value(
                self.inner
                    .host_rpc(
                        device_id,
                        crate::rpc::methods::READ_WORKSPACE_IMAGE,
                        serde_json::to_value(&request)
                            .map_err(|_| ClientError::Internal("Invalid image request".into()))?,
                    )
                    .await?,
            )
            .map_err(|_| ClientError::Internal("Invalid image response".into()))?;
            if chunk.checkout_id != request.expected_checkout_id
                || chunk.content_hash.is_empty()
                || chunk.size > MAX_WORKSPACE_IMAGE_BYTES
                || chunk.data.len() > WORKSPACE_IMAGE_CHUNK_BYTES.div_ceil(3) * 4
                || request
                    .expected_content_hash
                    .as_ref()
                    .is_some_and(|hash| hash != &chunk.content_hash)
                || mime.as_ref().is_some_and(|m| m != &chunk.mime_type)
                || size.is_some_and(|s| s != chunk.size)
            {
                return Err(ClientError::Internal(
                    "Image identity or size changed".into(),
                ));
            }
            let part = base64::engine::general_purpose::STANDARD
                .decode(&chunk.data)
                .map_err(|e| ClientError::Internal(e.to_string()))?;
            if part.is_empty()
                || request.offset.checked_add(part.len()) != Some(chunk.next_offset)
                || chunk.next_offset > chunk.size
                || chunk.done != (chunk.next_offset == chunk.size)
            {
                return Err(ClientError::Internal("Invalid image chunk offset".into()));
            }
            bytes.extend(part);
            if chunk.done {
                return Ok((chunk.mime_type, bytes));
            }
            request.offset = chunk.next_offset;
            request.expected_content_hash = Some(chunk.content_hash);
            mime = Some(chunk.mime_type);
            size = Some(chunk.size);
        }
        Err(ClientError::Internal("Image chunk limit exceeded".into()))
    }
}
