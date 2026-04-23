use std::sync::Arc;

use obix::{InboxEvent, InboxHandler, InboxResult};

use super::job::{WriteToRuntimeConfig, WRITE_TO_RUNTIME_QUEUE};
use super::search::SearchStore;

pub(super) struct LibraryWriteHandler {
    search: SearchStore,
    embedder: Arc<code_assistant_core::embedder::Embedder>,
    write_spawner: job::JobSpawner<WriteToRuntimeConfig>,
}

impl LibraryWriteHandler {
    pub fn new(
        search: SearchStore,
        embedder: Arc<code_assistant_core::embedder::Embedder>,
        write_spawner: job::JobSpawner<WriteToRuntimeConfig>,
    ) -> Self {
        Self {
            search,
            embedder,
            write_spawner,
        }
    }
}

impl InboxHandler for LibraryWriteHandler {
    async fn handle(
        &self,
        event: &InboxEvent,
    ) -> Result<InboxResult, Box<dyn std::error::Error + Send + Sync>> {
        let file: super::RuntimeFile = event.payload()?;

        let fields = file.searchable_fields();
        let text = format!("{}\n\n{}", fields.title, fields.body);
        let embedding = self.embedder.embed_document(&text).await?;
        self.search
            .set_embedding(fields.doc_id, fields.doc_type, embedding)
            .await?;

        let config = WriteToRuntimeConfig { file };
        self.write_spawner
            .spawn_with_queue_id(job::JobId::new(), config, WRITE_TO_RUNTIME_QUEUE)
            .await?;

        Ok(InboxResult::Complete)
    }
}
