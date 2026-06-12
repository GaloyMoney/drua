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
        let manifest = deployment_manifest(config, checksum, enabled);

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

fn deployment_manifest(
    config: &PostgresMcpConfig,
    checksum: &str,
    enabled: bool,
) -> serde_json::Value {
    let replicas = if enabled { 1 } else { 0 };
    let mut pod_spec = serde_json::Map::from_iter([
        (
            "automountServiceAccountToken".to_string(),
            serde_json::json!(config.cloud_sql_proxy.is_some()),
        ),
        (
            "containers".to_string(),
            serde_json::json!(dbhub_containers(config)),
        ),
        (
            "volumes".to_string(),
            serde_json::json!([{
                "name": "dbhub-config",
                "secret": {
                    "secretName": config.config_secret,
                    "items": [{
                        "key": POSTGRES_MCP_CONFIG_KEY,
                        "path": POSTGRES_MCP_CONFIG_KEY,
                    }],
                },
            }]),
        ),
    ]);

    if let Some(service_account_name) = &config.service_account_name {
        pod_spec.insert(
            "serviceAccountName".to_string(),
            serde_json::json!(service_account_name),
        );
    }

    serde_json::json!({
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
                "spec": pod_spec,
            },
        },
    })
}

fn dbhub_containers(config: &PostgresMcpConfig) -> Vec<serde_json::Value> {
    let mut containers = vec![serde_json::json!({
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
    })];

    if let Some(proxy) = &config.cloud_sql_proxy {
        if let Some(runtime_arg) = &proxy.runtime_arg {
            containers.push(cloud_sql_proxy_container(
                "cloud-sql-proxy-runtime",
                &proxy.image,
                &proxy.image_pull_policy,
                runtime_arg,
            ));
        }

        if let Some(datawarehouse_arg) = &proxy.datawarehouse_arg {
            containers.push(cloud_sql_proxy_container(
                "cloud-sql-proxy-datawarehouse",
                &proxy.image,
                &proxy.image_pull_policy,
                datawarehouse_arg,
            ));
        }
    }

    containers
}

fn cloud_sql_proxy_container(
    name: &str,
    image: &str,
    image_pull_policy: &str,
    instance_arg: &str,
) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "image": image,
        "imagePullPolicy": image_pull_policy,
        "args": [
            "--auto-iam-authn",
            "--structured-logs",
            instance_arg,
        ],
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::postgres_mcp::{
        PostgresMcpCloudSqlProxyConfig, PostgresMcpConfig, DEFAULT_POSTGRES_MCP_CONFIG_SECRET,
        DEFAULT_POSTGRES_MCP_IMAGE, DEFAULT_POSTGRES_MCP_LIMIT_CPU,
        DEFAULT_POSTGRES_MCP_LIMIT_MEMORY, DEFAULT_POSTGRES_MCP_MAX_ROWS,
        DEFAULT_POSTGRES_MCP_QUERY_TIMEOUT, DEFAULT_POSTGRES_MCP_REQUEST_CPU,
        DEFAULT_POSTGRES_MCP_REQUEST_MEMORY, DEFAULT_POSTGRES_MCP_UPSTREAM_NAME,
    };

    use super::deployment_manifest;

    #[test]
    fn renders_dbhub_deployment_without_service_account_or_proxy() {
        let manifest = deployment_manifest(&test_config(), "abc123", true);
        let pod_spec = &manifest["spec"]["template"]["spec"];

        assert_eq!(pod_spec["automountServiceAccountToken"], false);
        assert!(pod_spec.get("serviceAccountName").is_none());

        let containers = pod_spec["containers"].as_array().unwrap();
        assert_eq!(containers.len(), 1);
        assert_eq!(containers[0]["name"], "postgres-mcp");
    }

    #[test]
    fn renders_dbhub_deployment_with_cloud_sql_proxy_sidecars() {
        let mut config = test_config();
        config.service_account_name = Some("lana-postgres-mcp".to_string());
        config.cloud_sql_proxy = Some(PostgresMcpCloudSqlProxyConfig {
            image: "gcr.io/cloud-sql-connectors/cloud-sql-proxy:2.18.3".to_string(),
            image_pull_policy: "IfNotPresent".to_string(),
            runtime_arg: Some("project:region:runtime?port=5432&private-ip=true".to_string()),
            datawarehouse_arg: Some(
                "project:region:datawarehouse?port=5433&private-ip=true".to_string(),
            ),
        });

        let manifest = deployment_manifest(&config, "abc123", true);
        let pod_spec = &manifest["spec"]["template"]["spec"];

        assert_eq!(pod_spec["automountServiceAccountToken"], true);
        assert_eq!(pod_spec["serviceAccountName"], "lana-postgres-mcp");

        let containers = pod_spec["containers"].as_array().unwrap();
        let names = containers
            .iter()
            .map(|container| container["name"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            vec![
                "postgres-mcp",
                "cloud-sql-proxy-runtime",
                "cloud-sql-proxy-datawarehouse"
            ]
        );
        assert_eq!(
            containers[1]["args"],
            serde_json::json!([
                "--auto-iam-authn",
                "--structured-logs",
                "project:region:runtime?port=5432&private-ip=true"
            ])
        );
        assert_eq!(
            containers[2]["args"],
            serde_json::json!([
                "--auto-iam-authn",
                "--structured-logs",
                "project:region:datawarehouse?port=5433&private-ip=true"
            ])
        );
    }

    fn test_config() -> PostgresMcpConfig {
        PostgresMcpConfig {
            namespace: "test".to_string(),
            resource_name: "lana-postgres-mcp".to_string(),
            config_secret: DEFAULT_POSTGRES_MCP_CONFIG_SECRET.to_string(),
            upstream_name: DEFAULT_POSTGRES_MCP_UPSTREAM_NAME.to_string(),
            image: DEFAULT_POSTGRES_MCP_IMAGE.to_string(),
            image_pull_policy: "IfNotPresent".to_string(),
            service_port: 8000,
            query_timeout: DEFAULT_POSTGRES_MCP_QUERY_TIMEOUT,
            max_rows: DEFAULT_POSTGRES_MCP_MAX_ROWS,
            connect_timeout: Duration::from_secs(5),
            request_cpu: DEFAULT_POSTGRES_MCP_REQUEST_CPU.to_string(),
            request_memory: DEFAULT_POSTGRES_MCP_REQUEST_MEMORY.to_string(),
            limit_cpu: DEFAULT_POSTGRES_MCP_LIMIT_CPU.to_string(),
            limit_memory: DEFAULT_POSTGRES_MCP_LIMIT_MEMORY.to_string(),
            runtime_seed_dsn: "postgres://mcp:secret@postgres.local:5432/postgres".to_string(),
            datawarehouse_seed_dsn: None,
            service_account_name: None,
            cloud_sql_proxy: None,
        }
    }
}
