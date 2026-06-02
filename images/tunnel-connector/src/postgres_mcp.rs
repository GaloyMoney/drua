use std::collections::BTreeMap;

use async_trait::async_trait;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::sync::watch;

use crate::mcp_upstream::UpstreamConfig;

pub(crate) const DEFAULT_POSTGRES_MCP_REGISTRY_SECRET: &str = "lana-postgres-mcp-registry";
pub(crate) const DEFAULT_POSTGRES_MCP_REGISTRY_KEY: &str = "registry.yaml";
pub(crate) const DEFAULT_POSTGRES_MCP_RESOURCE_NAME: &str = "lana-postgres-mcp";
pub(crate) const DEFAULT_POSTGRES_MCP_CONFIG_SECRET: &str = "lana-postgres-mcp-config";
pub(crate) const DEFAULT_POSTGRES_MCP_UPSTREAM_NAME: &str = "lana_postgres";
pub(crate) const DEFAULT_POSTGRES_MCP_IMAGE: &str = "bytebase/dbhub:0.21.1";
pub(crate) const DEFAULT_POSTGRES_MCP_SERVICE_PORT: u16 = 8000;
pub(crate) const DEFAULT_POSTGRES_MCP_QUERY_TIMEOUT: u32 = 30;
pub(crate) const DEFAULT_POSTGRES_MCP_MAX_ROWS: u32 = 1000;
pub(crate) const DEFAULT_POSTGRES_MCP_CONNECT_TIMEOUT_SECS: u64 = 5;
pub(crate) const DEFAULT_POSTGRES_MCP_REQUEST_CPU: &str = "50m";
pub(crate) const DEFAULT_POSTGRES_MCP_REQUEST_MEMORY: &str = "64Mi";
pub(crate) const DEFAULT_POSTGRES_MCP_LIMIT_CPU: &str = "200m";
pub(crate) const DEFAULT_POSTGRES_MCP_LIMIT_MEMORY: &str = "256Mi";

#[derive(Clone, Debug)]
pub(crate) struct PostgresMcpConfig {
    pub(crate) namespace: String,
    pub(crate) registry_secret: String,
    pub(crate) registry_key: String,
    pub(crate) resource_name: String,
    pub(crate) config_secret: String,
    pub(crate) upstream_name: String,
    pub(crate) image: String,
    pub(crate) image_pull_policy: String,
    pub(crate) service_port: u16,
    pub(crate) query_timeout: u32,
    pub(crate) max_rows: u32,
    pub(crate) connect_timeout: std::time::Duration,
    pub(crate) request_cpu: String,
    pub(crate) request_memory: String,
    pub(crate) limit_cpu: String,
    pub(crate) limit_memory: String,
}

/// Outbound platform operations used by the Postgres MCP application service.
#[async_trait]
pub(crate) trait PostgresMcpHandler: Clone + Send + Sync + 'static {
    fn spawn_registry_watcher(&self, config: &PostgresMcpConfig) -> watch::Receiver<u64>;

    async fn read_registry(
        &self,
        config: &PostgresMcpConfig,
    ) -> anyhow::Result<BTreeMap<String, RegistryEntry>>;

    async fn apply_dbhub(
        &self,
        config: &PostgresMcpConfig,
        dbhub_toml: &str,
        checksum: &str,
        enabled: bool,
    ) -> anyhow::Result<()>;
}

/// Outbound credential probe used to omit unusable database sources.
#[async_trait]
pub(crate) trait PostgresSourceValidator: Clone + Send + Sync + 'static {
    async fn validate(
        &self,
        source: &PostgresSource,
        timeout: std::time::Duration,
    ) -> anyhow::Result<()>;
}

#[derive(Clone)]
pub(crate) struct PostgresMcpController<H, V> {
    handler: H,
    validator: V,
    config: PostgresMcpConfig,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RegistryEntry {
    runtime_pg_con: Option<String>,
    datawarehouse_pg_con: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PostgresSource {
    id: String,
    instance: String,
    role: PostgresSourceRole,
    pub(crate) dsn: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum PostgresSourceRole {
    Runtime,
    Datawarehouse,
}

impl PostgresSourceRole {
    fn suffix(&self) -> &'static str {
        match self {
            Self::Runtime => "runtime",
            Self::Datawarehouse => "datawarehouse",
        }
    }

    fn description(&self, instance: &str) -> String {
        match self {
            Self::Runtime => format!(
                "Lana Bank {instance} runtime PostgreSQL database. Use for operational app state, public outbox, jobs, users, and domain tables."
            ),
            Self::Datawarehouse => format!(
                "Lana Bank {instance} reporting data warehouse PostgreSQL database. Use for dbt source, staging, intermediate, and report output relations."
            ),
        }
    }
}

impl<H, V> PostgresMcpController<H, V>
where
    H: PostgresMcpHandler,
    V: PostgresSourceValidator,
{
    pub(crate) fn try_new(
        config: PostgresMcpConfig,
        handler: H,
        validator: V,
    ) -> anyhow::Result<Self> {
        config.validate()?;

        Ok(Self {
            handler,
            validator,
            config,
        })
    }

    pub(crate) fn upstream_name(&self) -> &str {
        &self.config.upstream_name
    }

    pub(crate) fn spawn_registry_watcher(&self) -> watch::Receiver<u64> {
        self.handler.spawn_registry_watcher(&self.config)
    }

    pub(crate) async fn reconcile(&self) -> anyhow::Result<Option<UpstreamConfig>> {
        let registry = self.handler.read_registry(&self.config).await?;
        let candidates = build_postgres_sources(&registry);
        let valid_sources = self.validate_sources(candidates).await;
        let dbhub_toml = render_dbhub_toml(
            &valid_sources,
            self.config.query_timeout,
            self.config.max_rows,
        );
        let checksum = sha256_hex(dbhub_toml.as_bytes());

        self.handler
            .apply_dbhub(
                &self.config,
                &dbhub_toml,
                &checksum,
                !valid_sources.is_empty(),
            )
            .await?;

        if valid_sources.is_empty() {
            tracing::warn!(
                "postgres mcp registry has no valid sources; aggregate DBHub scaled to zero"
            );
            return Ok(None);
        }

        Ok(Some(UpstreamConfig {
            name: self.config.upstream_name.clone(),
            url: format!(
                "http://{}:{}/mcp",
                self.config.resource_name, self.config.service_port
            ),
        }))
    }

    async fn validate_sources(&self, sources: Vec<PostgresSource>) -> Vec<PostgresSource> {
        let mut valid_sources = Vec::with_capacity(sources.len());

        for source in sources {
            match self
                .validator
                .validate(&source, self.config.connect_timeout)
                .await
            {
                Ok(()) => valid_sources.push(source),
                Err(e) => {
                    tracing::warn!(
                        instance = %source.instance,
                        source = %source.id,
                        error = %e,
                        "omitting unusable postgres mcp source"
                    );
                }
            }
        }

        valid_sources
    }
}

impl PostgresMcpConfig {
    fn validate(&self) -> anyhow::Result<()> {
        for (field, value) in [
            ("registry_secret", &self.registry_secret),
            ("registry_key", &self.registry_key),
            ("resource_name", &self.resource_name),
            ("config_secret", &self.config_secret),
            ("upstream_name", &self.upstream_name),
            ("image", &self.image),
        ] {
            if value.trim().is_empty() {
                anyhow::bail!("postgres mcp {field} must not be empty");
            }
        }

        if self.service_port == 0 {
            anyhow::bail!("postgres mcp service port must be non-zero");
        }

        Ok(())
    }
}

pub(crate) fn parse_registry_yaml(raw: &str) -> anyhow::Result<BTreeMap<String, RegistryEntry>> {
    let root: serde_yaml::Value = serde_yaml::from_str(raw)?;
    let instances = match root {
        serde_yaml::Value::Mapping(mut mapping) => mapping
            .remove(serde_yaml::Value::String("instances".to_string()))
            .unwrap_or(serde_yaml::Value::Mapping(mapping)),
        _ => anyhow::bail!("postgres mcp registry must be a YAML mapping"),
    };

    let registry: BTreeMap<String, RegistryEntry> = serde_yaml::from_value(instances)?;
    Ok(registry)
}

fn build_postgres_sources(registry: &BTreeMap<String, RegistryEntry>) -> Vec<PostgresSource> {
    let mut sources = Vec::new();

    for (instance, entry) in registry {
        if let Err(e) = validate_instance_name(instance) {
            tracing::warn!(
                instance = %instance,
                error = %e,
                "omitting postgres mcp registry instance with invalid name"
            );
            continue;
        }

        let Some(runtime_dsn) = entry
            .runtime_pg_con
            .as_deref()
            .map(str::trim)
            .filter(|dsn| !dsn.is_empty())
        else {
            tracing::warn!(
                instance = %instance,
                "omitting postgres mcp registry instance missing runtime_pg_con"
            );
            continue;
        };

        sources.push(PostgresSource {
            id: source_id(instance, PostgresSourceRole::Runtime),
            instance: instance.clone(),
            role: PostgresSourceRole::Runtime,
            dsn: runtime_dsn.to_string(),
        });

        if let Some(dw_dsn) = entry
            .datawarehouse_pg_con
            .as_deref()
            .map(str::trim)
            .filter(|dsn| !dsn.is_empty())
        {
            sources.push(PostgresSource {
                id: source_id(instance, PostgresSourceRole::Datawarehouse),
                instance: instance.clone(),
                role: PostgresSourceRole::Datawarehouse,
                dsn: dw_dsn.to_string(),
            });
        }
    }

    sources
}

fn validate_instance_name(instance: &str) -> anyhow::Result<()> {
    let mut chars = instance.chars();
    let Some(first) = chars.next() else {
        anyhow::bail!("postgres mcp instance name must not be empty");
    };

    if !first.is_ascii_lowercase() {
        anyhow::bail!(
            "postgres mcp instance name {instance:?} must start with a lowercase ASCII letter"
        );
    }

    if !chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') {
        anyhow::bail!("postgres mcp instance name {instance:?} must match ^[a-z][a-z0-9_]*$");
    }

    Ok(())
}

fn source_id(instance: &str, role: PostgresSourceRole) -> String {
    format!("{}_{}", instance, role.suffix())
}

fn render_dbhub_toml(sources: &[PostgresSource], query_timeout: u32, max_rows: u32) -> String {
    let mut rendered = String::new();

    for source in sources {
        if !rendered.is_empty() {
            rendered.push('\n');
        }

        rendered.push_str("[[sources]]\n");
        rendered.push_str(&format!("id = \"{}\"\n", toml_escape(&source.id)));
        rendered.push_str(&format!(
            "description = \"{}\"\n",
            toml_escape(&source.role.description(&source.instance))
        ));
        rendered.push_str(&format!("dsn = \"{}\"\n", toml_escape(&source.dsn)));
        rendered.push_str("lazy = true\n");
        rendered.push_str(&format!("query_timeout = {query_timeout}\n\n"));
        rendered.push_str("[[tools]]\n");
        rendered.push_str("name = \"execute_sql\"\n");
        rendered.push_str(&format!("source = \"{}\"\n", toml_escape(&source.id)));
        rendered.push_str("readonly = true\n");
        rendered.push_str(&format!("max_rows = {max_rows}\n\n"));
        rendered.push_str("[[tools]]\n");
        rendered.push_str("name = \"search_objects\"\n");
        rendered.push_str(&format!("source = \"{}\"\n", toml_escape(&source.id)));
        rendered.push('\n');
    }

    rendered
}

fn toml_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_registry_with_instance_names_as_top_level_keys() {
        let registry = parse_registry_yaml(
            r#"
main:
  runtime_pg_con: "postgres://runtime"
  datawarehouse_pg_con: "postgres://dw"
vanilla:
  runtime_pg_con: "postgres://vanilla"
"#,
        )
        .unwrap();

        let sources = build_postgres_sources(&registry);

        assert_eq!(
            sources
                .iter()
                .map(|source| source.id.as_str())
                .collect::<Vec<_>>(),
            vec!["main_runtime", "main_datawarehouse", "vanilla_runtime"]
        );
    }

    #[test]
    fn parses_registry_with_instances_wrapper_for_forward_compatibility() {
        let registry = parse_registry_yaml(
            r#"
instances:
  main:
    runtime_pg_con: "postgres://runtime"
"#,
        )
        .unwrap();

        let sources = build_postgres_sources(&registry);

        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].id, "main_runtime");
    }

    #[test]
    fn omits_instance_names_that_would_make_unstable_tool_names() {
        let registry = parse_registry_yaml(
            r#"
Main:
  runtime_pg_con: "postgres://runtime"
"#,
        )
        .unwrap();

        let sources = build_postgres_sources(&registry);

        assert!(sources.is_empty());
    }

    #[test]
    fn renders_dbhub_toml_with_unique_source_ids() {
        let sources = vec![
            PostgresSource {
                id: "main_runtime".to_string(),
                instance: "main".to_string(),
                role: PostgresSourceRole::Runtime,
                dsn: "postgres://runtime".to_string(),
            },
            PostgresSource {
                id: "main_datawarehouse".to_string(),
                instance: "main".to_string(),
                role: PostgresSourceRole::Datawarehouse,
                dsn: "postgres://dw".to_string(),
            },
        ];

        let rendered = render_dbhub_toml(&sources, 30, 1000);

        assert!(rendered.contains("id = \"main_runtime\""));
        assert!(rendered.contains("source = \"main_runtime\""));
        assert!(rendered.contains("id = \"main_datawarehouse\""));
        assert!(rendered.contains("source = \"main_datawarehouse\""));
        assert!(rendered.contains("readonly = true"));
        assert!(rendered.contains("max_rows = 1000"));
    }
}
