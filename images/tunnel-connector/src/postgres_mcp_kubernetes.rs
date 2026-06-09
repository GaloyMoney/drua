use std::collections::BTreeMap;

use async_trait::async_trait;
use base64::Engine;
use k8s_openapi::api::{
    apps::v1::Deployment,
    core::v1::{Secret, Service},
};
use kube::{
    api::{Api, Patch, PatchParams},
    Client,
};

use crate::postgres_mcp::{PostgresMcpConfig, PostgresMcpHandler};

const POSTGRES_MCP_FIELD_MANAGER: &str = "tunnel-connector";
const POSTGRES_MCP_CONFIG_KEY: &str = "dbhub.toml";
const SERVICE_ACCOUNT_NAMESPACE_PATH: &str =
    "/var/run/secrets/kubernetes.io/serviceaccount/namespace";

#[derive(Clone)]
pub(crate) struct KubernetesPostgresMcpHandler {
    client: Client,
}

impl KubernetesPostgresMcpHandler {
    pub(crate) async fn try_default() -> anyhow::Result<Self> {
        Ok(Self {
            client: Client::try_default().await?,
        })
    }

    async fn apply_config_secret(
        &self,
        config: &PostgresMcpConfig,
        dbhub_toml: &str,
        checksum: &str,
    ) -> anyhow::Result<()> {
        let secrets: Api<Secret> = Api::namespaced(self.client.clone(), &config.namespace);
        let manifest = serde_json::json!({
            "apiVersion": "v1",
            "kind": "Secret",
            "metadata": {
                "name": config.config_secret,
                "namespace": config.namespace,
                "labels": managed_labels("config"),
                "annotations": {
                    "checksum/postgres-mcp-config": checksum,
                },
            },
            "type": "Opaque",
            "data": {
                POSTGRES_MCP_CONFIG_KEY: base64::engine::general_purpose::STANDARD.encode(dbhub_toml.as_bytes()),
            },
        });

        secrets
            .patch(
                &config.config_secret,
                &apply_params(),
                &Patch::Apply(manifest),
            )
            .await?;

        Ok(())
    }

    async fn apply_service(&self, config: &PostgresMcpConfig) -> anyhow::Result<()> {
        let services: Api<Service> = Api::namespaced(self.client.clone(), &config.namespace);
        let manifest = serde_json::json!({
            "apiVersion": "v1",
            "kind": "Service",
            "metadata": {
                "name": config.resource_name,
                "namespace": config.namespace,
                "labels": managed_labels("dbhub"),
            },
            "spec": {
                "type": "ClusterIP",
                "ports": [{
                    "name": "http",
                    "port": config.service_port,
                    "targetPort": config.service_port,
                    "protocol": "TCP",
                }],
                "selector": dbhub_selector_labels(),
            },
        });

        services
            .patch(
                &config.resource_name,
                &apply_params(),
                &Patch::Apply(manifest),
            )
            .await?;

        Ok(())
    }

    async fn apply_deployment(
        &self,
        config: &PostgresMcpConfig,
        checksum: &str,
        enabled: bool,
    ) -> anyhow::Result<()> {
        let deployments: Api<Deployment> = Api::namespaced(self.client.clone(), &config.namespace);
        let replicas = if enabled { 1 } else { 0 };
        let manifest = serde_json::json!({
            "apiVersion": "apps/v1",
            "kind": "Deployment",
            "metadata": {
                "name": config.resource_name,
                "namespace": config.namespace,
                "labels": managed_labels("dbhub"),
            },
            "spec": {
                "replicas": replicas,
                "selector": {
                    "matchLabels": dbhub_selector_labels(),
                },
                "template": {
                    "metadata": {
                        "labels": managed_labels("dbhub"),
                        "annotations": {
                            "checksum/postgres-mcp-config": checksum,
                        },
                    },
                    "spec": {
                        "automountServiceAccountToken": false,
                        "containers": [{
                            "name": "postgres-mcp",
                            "image": config.image,
                            "imagePullPolicy": config.image_pull_policy,
                            "args": [
                                "--transport=http",
                                format!("--port={}", config.service_port),
                                format!("--config=/etc/dbhub/{POSTGRES_MCP_CONFIG_KEY}"),
                            ],
                            "ports": [{
                                "name": "http",
                                "containerPort": config.service_port,
                                "protocol": "TCP",
                            }],
                            "volumeMounts": [{
                                "name": "dbhub-config",
                                "mountPath": "/etc/dbhub",
                                "readOnly": true,
                            }],
                            "resources": {
                                "requests": {
                                    "cpu": config.request_cpu,
                                    "memory": config.request_memory,
                                },
                                "limits": {
                                    "cpu": config.limit_cpu,
                                    "memory": config.limit_memory,
                                },
                            },
                        }],
                        "volumes": [{
                            "name": "dbhub-config",
                            "secret": {
                                "secretName": config.config_secret,
                                "items": [{
                                    "key": POSTGRES_MCP_CONFIG_KEY,
                                    "path": POSTGRES_MCP_CONFIG_KEY,
                                }],
                            },
                        }],
                    },
                },
            },
        });

        deployments
            .patch(
                &config.resource_name,
                &apply_params(),
                &Patch::Apply(manifest),
            )
            .await?;

        Ok(())
    }
}

#[async_trait]
impl PostgresMcpHandler for KubernetesPostgresMcpHandler {
    async fn apply_dbhub(
        &self,
        config: &PostgresMcpConfig,
        dbhub_toml: &str,
        checksum: &str,
        enabled: bool,
    ) -> anyhow::Result<()> {
        self.apply_config_secret(config, dbhub_toml, checksum)
            .await?;
        self.apply_service(config).await?;
        self.apply_deployment(config, checksum, enabled).await?;

        Ok(())
    }
}

pub(crate) fn resolve_namespace(configured: Option<String>) -> anyhow::Result<String> {
    match configured {
        Some(namespace) if !namespace.trim().is_empty() => Ok(namespace),
        _ => current_kubernetes_namespace().ok_or_else(|| {
            anyhow::anyhow!("TUNNEL_POSTGRES_MCP_NAMESPACE is required outside Kubernetes")
        }),
    }
}

fn current_kubernetes_namespace() -> Option<String> {
    std::fs::read_to_string(SERVICE_ACCOUNT_NAMESPACE_PATH)
        .ok()
        .map(|namespace| namespace.trim().to_string())
        .filter(|namespace| !namespace.is_empty())
}

fn managed_labels(component: &str) -> BTreeMap<&'static str, String> {
    BTreeMap::from([
        (
            "app.kubernetes.io/managed-by",
            "tunnel-connector".to_string(),
        ),
        ("app.kubernetes.io/name", "lana-postgres-mcp".to_string()),
        ("app.kubernetes.io/component", component.to_string()),
        ("mcp.galoy.io/type", "postgres".to_string()),
    ])
}

fn dbhub_selector_labels() -> BTreeMap<&'static str, String> {
    BTreeMap::from([
        (
            "app.kubernetes.io/managed-by",
            "tunnel-connector".to_string(),
        ),
        ("app.kubernetes.io/name", "lana-postgres-mcp".to_string()),
        ("app.kubernetes.io/component", "dbhub".to_string()),
        ("mcp.galoy.io/type", "postgres".to_string()),
    ])
}

fn apply_params() -> PatchParams {
    PatchParams::apply(POSTGRES_MCP_FIELD_MANAGER).force()
}
