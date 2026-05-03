use std::sync::Arc;

use job::{CurrentJob, Job, JobCompletion, JobInitializer, JobRunner, JobSpawner, JobType};
use serde::{Deserialize, Serialize};

use crate::importer::DocType;
use crate::search::SearchStore;

pub(crate) const LIBRARY_EMBED_JOB: &str = "library.embed";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct LibraryEmbedConfig {
    pub doc_id: uuid::Uuid,
    pub doc_type: DocType,
}

pub(crate) struct LibraryEmbedJobInitializer {
    search: SearchStore,
    embedder: Arc<code_assistant_core::embedder::Embedder>,
}

impl LibraryEmbedJobInitializer {
    pub fn new(
        search: SearchStore,
        embedder: Arc<code_assistant_core::embedder::Embedder>,
    ) -> Self {
        Self { search, embedder }
    }
}

impl JobInitializer for LibraryEmbedJobInitializer {
    type Config = LibraryEmbedConfig;

    fn job_type(&self) -> JobType {
        JobType::new(LIBRARY_EMBED_JOB)
    }

    fn init(
        &self,
        job: &Job,
        _spawner: JobSpawner<Self::Config>,
    ) -> Result<Box<dyn JobRunner>, Box<dyn std::error::Error>> {
        let config: LibraryEmbedConfig = job.config()?;
        Ok(Box::new(LibraryEmbedRunner {
            search: self.search.clone(),
            embedder: Arc::clone(&self.embedder),
            config,
        }))
    }
}

struct LibraryEmbedRunner {
    search: SearchStore,
    embedder: Arc<code_assistant_core::embedder::Embedder>,
    config: LibraryEmbedConfig,
}

#[async_trait::async_trait]
impl JobRunner for LibraryEmbedRunner {
    #[tracing::instrument(
        name = "library.embed.run",
        skip_all,
        fields(doc_id = %self.config.doc_id, doc_type = %self.config.doc_type),
    )]
    async fn run(
        &self,
        _current_job: CurrentJob,
    ) -> Result<JobCompletion, Box<dyn std::error::Error>> {
        let Some(fields) = self
            .search
            .find_by_id(self.config.doc_id, &self.config.doc_type)
            .await?
        else {
            tracing::debug!("embed: doc gone, skipping");
            return Ok(JobCompletion::Complete);
        };

        let embedding = self.embedder.embed_document(&fields.content).await?;
        self.search
            .set_embedding(self.config.doc_id, &self.config.doc_type, embedding)
            .await?;

        Ok(JobCompletion::Complete)
    }
}
