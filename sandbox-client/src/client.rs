use kube::api::{Api, DeleteParams, ListParams, PostParams};
use kube::Client;
use tracing::instrument;

use crate::error::SandboxError;
use crate::types::*;

/// High-level client for managing Agent Sandbox resources.
#[derive(Clone)]
pub struct SandboxClient {
    client: Client,
    namespace: String,
    template_name: String,
}

/// Summary of a sandbox for API consumers.
#[derive(Clone, Debug, serde::Serialize)]
pub struct SandboxSummary {
    pub name: String,
    pub sandbox_name: Option<String>,
    pub phase: String,
    pub ready: bool,
}

impl SandboxClient {
    pub fn new(client: Client, namespace: String, template_name: String) -> Self {
        Self {
            client,
            namespace,
            template_name,
        }
    }

    /// Build a client from in-cluster or kubeconfig environment.
    pub async fn try_from_env(
        namespace: String,
        template_name: String,
    ) -> Result<Self, SandboxError> {
        let client = Client::try_default().await?;
        Ok(Self::new(client, namespace, template_name))
    }

    /// Create a SandboxClaim that allocates a sandbox from the warm pool.
    #[instrument(name = "sandbox_client.create_claim", skip_all, fields(%claim_name))]
    pub async fn create_claim(&self, claim_name: &str) -> Result<SandboxClaim, SandboxError> {
        let claims: Api<SandboxClaim> = Api::namespaced(self.client.clone(), &self.namespace);

        let claim = SandboxClaim::new(
            claim_name,
            SandboxClaimSpec {
                template_ref: TemplateRef {
                    name: self.template_name.clone(),
                },
                lifecycle: None,
            },
        );

        let created = claims.create(&PostParams::default(), &claim).await?;
        tracing::info!(claim = %claim_name, "Sandbox claim created");
        Ok(created)
    }

    /// Delete a SandboxClaim, releasing the sandbox back.
    #[instrument(name = "sandbox_client.delete_claim", skip_all, fields(%claim_name))]
    pub async fn delete_claim(&self, claim_name: &str) -> Result<(), SandboxError> {
        let claims: Api<SandboxClaim> = Api::namespaced(self.client.clone(), &self.namespace);
        claims.delete(claim_name, &DeleteParams::default()).await?;
        tracing::info!(claim = %claim_name, "Sandbox claim deleted");
        Ok(())
    }

    /// List all SandboxClaims in the namespace.
    #[instrument(name = "sandbox_client.list_claims", skip_all)]
    pub async fn list_claims(&self) -> Result<Vec<SandboxSummary>, SandboxError> {
        let claims: Api<SandboxClaim> = Api::namespaced(self.client.clone(), &self.namespace);
        let list = claims.list(&ListParams::default()).await?;

        let summaries = list
            .items
            .iter()
            .map(|claim| {
                let name = claim
                    .metadata
                    .name
                    .clone()
                    .unwrap_or_else(|| "<unknown>".into());
                let status = claim.status.as_ref();
                let sandbox_name = status.and_then(|s| s.sandbox_name.clone());
                let ready = status
                    .map(|s| {
                        s.conditions.iter().any(|c| {
                            c.get("type")
                                .and_then(|t| t.as_str())
                                .map(|t| t == "Ready")
                                .unwrap_or(false)
                                && c.get("status")
                                    .and_then(|s| s.as_str())
                                    .map(|s| s == "True")
                                    .unwrap_or(false)
                        })
                    })
                    .unwrap_or(false);

                let phase = if ready {
                    "Ready".to_string()
                } else if sandbox_name.is_some() {
                    "Provisioning".to_string()
                } else {
                    "Pending".to_string()
                };

                SandboxSummary {
                    name,
                    sandbox_name,
                    phase,
                    ready,
                }
            })
            .collect();

        Ok(summaries)
    }

    /// Get a single SandboxClaim by name.
    #[instrument(name = "sandbox_client.get_claim", skip_all, fields(%claim_name))]
    pub async fn get_claim(&self, claim_name: &str) -> Result<SandboxSummary, SandboxError> {
        let claims: Api<SandboxClaim> = Api::namespaced(self.client.clone(), &self.namespace);
        let claim = claims.get(claim_name).await.map_err(|e| match &e {
            kube::Error::Api(resp) if resp.code == 404 => {
                SandboxError::NotFound(claim_name.to_string())
            }
            _ => SandboxError::Kube(e),
        })?;

        let status = claim.status.as_ref();
        let sandbox_name = status.and_then(|s| s.sandbox_name.clone());
        let ready = status
            .map(|s| {
                s.conditions.iter().any(|c| {
                    c.get("type")
                        .and_then(|t| t.as_str())
                        .map(|t| t == "Ready")
                        .unwrap_or(false)
                        && c.get("status")
                            .and_then(|s| s.as_str())
                            .map(|s| s == "True")
                            .unwrap_or(false)
                })
            })
            .unwrap_or(false);

        let phase = if ready {
            "Ready".to_string()
        } else if sandbox_name.is_some() {
            "Provisioning".to_string()
        } else {
            "Pending".to_string()
        };

        Ok(SandboxSummary {
            name: claim_name.to_string(),
            sandbox_name,
            phase,
            ready,
        })
    }
}
