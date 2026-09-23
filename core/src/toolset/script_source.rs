//! Caller-bound adapter to ordinary authorized space reads.

use std::sync::{Arc, OnceLock};

use drua_library::SpaceError;

use crate::{auth::AuthSubject, project::ProjectError, space_fs::SpaceFs};

struct SpaceScriptProvider {
    fs: Arc<SpaceFs>,
    subject: AuthSubject,
}

#[derive(Default)]
pub struct ScriptProviderFactory(OnceLock<Arc<SpaceFs>>);

impl ScriptProviderFactory {
    pub fn initialize(&self, fs: Arc<SpaceFs>) {
        assert!(
            self.0.set(fs).is_ok(),
            "script provider already initialized"
        );
    }

    pub fn for_subject(
        &self,
        subject: &AuthSubject,
    ) -> Option<Arc<dyn js_engine::ScriptSourceProvider>> {
        Some(Arc::new(SpaceScriptProvider {
            fs: self.0.get()?.clone(),
            subject: subject.clone(),
        }))
    }
}

#[async_trait::async_trait]
impl js_engine::ScriptSourceProvider for SpaceScriptProvider {
    async fn read(&self, path: &str) -> Result<Vec<u8>, String> {
        self.fs
            .read_file(&self.subject, path)
            .await
            .map_err(|error| match error {
                ProjectError::Authorization(_)
                | ProjectError::Space(SpaceError::NotMounted { .. }) => {
                    "access_denied: space is not accessible".to_owned()
                }
                ProjectError::Space(
                    SpaceError::PathNotFound { .. } | SpaceError::NotFound { .. },
                ) => {
                    format!("missing_file: {error}")
                }
                _ => format!("read_error: {error}"),
            })?
            .ok_or_else(|| "invalid_path: expected a space path".to_owned())
    }
}
