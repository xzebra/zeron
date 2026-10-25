//! Project artwork fetched from its owning host; never persisted as a local file.
use crate::error::Result;
use crate::{Client, ClientError};
use base64::Engine as _;
use zeron_proto::{MAX_WORKSPACE_IMAGE_BYTES, WORKSPACE_IMAGE_CHUNK_BYTES, WorkspaceTarget};

impl Client {
    /// Repository artwork candidates for the native project editor.
    pub async fn project_images(&self, space_id: &str, query: &str) -> Result<Vec<String>> {
        let (device_id, target) = self.project_artwork_target(space_id)?;
        let mut paths = Vec::new();
        let mut failure = None;
        for query in zeron_proto::project_icon::browse_queries(query) {
            let request = zeron_proto::SearchWorkspaceFilesRequest {
                target: target.clone(),
                query,
                include_ignored: false,
                limit: Some(200),
            };
            let response = self
                .inner
                .host_rpc(
                    &device_id,
                    crate::rpc::methods::SEARCH_WORKSPACE_FILES,
                    serde_json::to_value(request)
                        .map_err(|_| ClientError::InvalidArgument("Invalid image search".into()))?,
                )
                .await;
            match response {
                Ok(value) => {
                    let matches: Vec<zeron_proto::WorkspaceFileSearchMatch> =
                        serde_json::from_value(value).map_err(|_| {
                            ClientError::Internal("Invalid image search response".into())
                        })?;
                    paths.extend(
                        matches
                            .into_iter()
                            .filter(|file| file.kind == zeron_proto::WorkspaceEntryKind::File)
                            .map(|file| file.path),
                    );
                }
                Err(error) => failure = Some(error),
            }
        }
        if paths.is_empty()
            && let Some(error) = failure
        {
            return Err(error);
        }
        Ok(zeron_proto::project_icon::rank_browse_results(query, paths))
    }

    fn project_artwork_target(&self, space_id: &str) -> Result<(String, WorkspaceTarget)> {
        let state = self.inner.workspace.state().0;
        let space = state
            .spaces
            .iter()
            .find(|space| space.id == space_id)
            .ok_or_else(|| ClientError::NotFound("Project".into()))?;
        Ok((
            space.device_id.clone(),
            WorkspaceTarget {
                chat_id: None,
                space_id: Some(space.id.clone()),
                checkout_path: None,
            },
        ))
    }

    /// Preview a selection without saving it. `None` resolves automatic artwork.
    pub async fn project_image(
        &self,
        space_id: &str,
        path: Option<&str>,
    ) -> Result<(String, Vec<u8>)> {
        let icon_path = zeron_proto::project_icon::normalize_path(path)
            .map_err(|error| ClientError::InvalidArgument(error.into()))?;
        let (device_id, target) = self.project_artwork_target(space_id)?;
        for path in zeron_proto::project_icon::candidates(icon_path.as_deref()) {
            let response = self
                .inner
                .host_rpc(
                    &device_id,
                    crate::rpc::methods::READ_WORKSPACE_FILE,
                    serde_json::to_value(zeron_proto::ReadWorkspaceFileRequest {
                        target: target.clone(),
                        path: path.into(),
                    })
                    .map_err(|_| ClientError::InvalidArgument("Invalid image request".into()))?,
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
                .project_icon_image(&device_id, target, path.into(), file.checkout_id)
                .await;
        }
        Err(ClientError::NotFound("Project artwork".into()))
    }
    /// The project's saved selection, resolved through its owning host.
    pub async fn project_icon(&self, space_id: &str) -> Result<(String, Vec<u8>)> {
        let icon_path = {
            let state = self.inner.workspace.state().0;
            let space = state
                .spaces
                .iter()
                .find(|space| space.id == space_id)
                .ok_or_else(|| ClientError::NotFound("Project".into()))?;
            zeron_proto::view::representative_space(&state.spaces, space)
                .icon_path
                .clone()
        };
        self.project_image(space_id, icon_path.as_deref()).await
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
