use async_trait::async_trait;
use sha2::{Digest, Sha256};

use crate::mcp_upstream::UpstreamConfig;

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
    pub(crate) runtime_seed_dsn: String,
    pub(crate) datawarehouse_seed_dsn: Option<String>,
}

/// Outbound Kubernetes operations used by the Postgres MCP application service.
#[async_trait]
pub(crate) trait PostgresMcpHandler: Clone + Send + Sync + 'static {
    async fn apply_dbhub(
        &self,
        config: &PostgresMcpConfig,
        dbhub_toml: &str,
        checksum: &str,
        enabled: bool,
    ) -> anyhow::Result<()>;
}

/// Outbound Postgres discovery port. Implementations must omit unusable
/// databases and return the still-valid set; a bad database should not break the
/// whole aggregate DBHub toolset.
#[async_trait]
pub(crate) trait PostgresSourceDiscoverer: Clone + Send + Sync + 'static {
    async fn discover_sources(
        &self,
        config: &PostgresMcpConfig,
    ) -> anyhow::Result<Vec<PostgresSource>>;
}

#[derive(Clone)]
pub(crate) struct PostgresMcpController<H, D> {
    handler: H,
    discoverer: D,
    config: PostgresMcpConfig,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PostgresSource {
    pub(crate) id: String,
    pub(crate) instance: String,
    pub(crate) database_oid: u32,
    role: PostgresSourceRole,
    pub(crate) dsn: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PostgresSourceRole {
    Runtime,
    Datawarehouse,
}

impl PostgresSourceRole {
    pub(crate) fn suffix(&self) -> &'static str {
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

impl PostgresSource {
    pub(crate) fn from_database_name(
        database_name: &str,
        role: PostgresSourceRole,
        dsn: String,
        database_oid: u32,
    ) -> Option<Self> {
        let instance = instance_from_database_name(database_name, &role)?;
        let source_instance = normalize_source_component(&instance)?;
        let id = format!("{}_{}", source_instance, role.suffix());

        Some(Self {
            id,
            instance,
            database_oid,
            role,
            dsn,
        })
    }
}

impl<H, D> PostgresMcpController<H, D>
where
    H: PostgresMcpHandler,
    D: PostgresSourceDiscoverer,
{
    pub(crate) fn try_new(
        config: PostgresMcpConfig,
        handler: H,
        discoverer: D,
    ) -> anyhow::Result<Self> {
        config.validate()?;

        Ok(Self {
            handler,
            discoverer,
            config,
        })
    }

    pub(crate) fn upstream_name(&self) -> &str {
        &self.config.upstream_name
    }

    pub(crate) async fn reconcile(&self) -> anyhow::Result<Option<UpstreamConfig>> {
        let sources = self.discoverer.discover_sources(&self.config).await?;
        let dbhub_toml =
            render_dbhub_toml(&sources, self.config.query_timeout, self.config.max_rows);
        let checksum = sha256_hex(dbhub_toml.as_bytes());

        self.handler
            .apply_dbhub(&self.config, &dbhub_toml, &checksum, !sources.is_empty())
            .await?;

        if sources.is_empty() {
            tracing::warn!(
                "postgres mcp discovery found no valid sources; aggregate DBHub scaled to zero"
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
}

impl PostgresMcpConfig {
    fn validate(&self) -> anyhow::Result<()> {
        for (field, value) in [
            ("resource_name", &self.resource_name),
            ("config_secret", &self.config_secret),
            ("upstream_name", &self.upstream_name),
            ("image", &self.image),
            ("runtime_seed_dsn", &self.runtime_seed_dsn),
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

fn instance_from_database_name(database_name: &str, role: &PostgresSourceRole) -> Option<String> {
    match role {
        PostgresSourceRole::Runtime => {
            if database_name == "lana-bank" {
                return Some("main".to_string());
            }

            database_name
                .strip_prefix("lana-bank-lana-bank-")
                .or_else(|| database_name.strip_prefix("lana-bank-"))
                .map(str::to_string)
        }
        PostgresSourceRole::Datawarehouse => {
            if database_name == "datawarehouse" {
                return Some("main".to_string());
            }

            database_name
                .strip_prefix("datawarehouse-lana-bank-")
                .or_else(|| database_name.strip_prefix("datawarehouse-"))
                .map(str::to_string)
        }
    }
}

fn normalize_source_component(value: &str) -> Option<String> {
    let normalized = value.replace('-', "_");
    let mut chars = normalized.chars();
    let first = chars.next()?;

    if !first.is_ascii_lowercase() {
        return None;
    }

    if !chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') {
        return None;
    }

    Some(normalized)
}

fn render_dbhub_toml(sources: &[PostgresSource], query_timeout: u32, max_rows: u32) -> String {
    let mut rendered = String::new();

    for source in sources {
        if !rendered.is_empty() {
            rendered.push('\n');
        }

        rendered.push_str("[[sources]]\n");
        rendered.push_str(&format!("# database_oid = {}\n", source.database_oid));
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
    fn derives_stable_sources_from_current_lana_database_names() {
        let runtime = PostgresSource::from_database_name(
            "lana-bank-lana-bank-main",
            PostgresSourceRole::Runtime,
            "postgres://runtime".to_string(),
            1,
        )
        .unwrap();
        let dw = PostgresSource::from_database_name(
            "datawarehouse-lana-bank-vanilla",
            PostgresSourceRole::Datawarehouse,
            "postgres://dw".to_string(),
            2,
        )
        .unwrap();

        assert_eq!(runtime.instance, "main");
        assert_eq!(runtime.id, "main_runtime");
        assert_eq!(dw.instance, "vanilla");
        assert_eq!(dw.id, "vanilla_datawarehouse");
    }

    #[test]
    fn supports_legacy_main_database_names() {
        let runtime = PostgresSource::from_database_name(
            "lana-bank",
            PostgresSourceRole::Runtime,
            "postgres://runtime".to_string(),
            1,
        )
        .unwrap();
        let dw = PostgresSource::from_database_name(
            "datawarehouse",
            PostgresSourceRole::Datawarehouse,
            "postgres://dw".to_string(),
            2,
        )
        .unwrap();

        assert_eq!(runtime.id, "main_runtime");
        assert_eq!(dw.id, "main_datawarehouse");
    }

    #[test]
    fn omits_unrecognized_database_names() {
        assert!(PostgresSource::from_database_name(
            "postgres",
            PostgresSourceRole::Runtime,
            "postgres://seed".to_string(),
            1,
        )
        .is_none());
    }

    #[test]
    fn renders_dbhub_toml_with_unique_source_ids() {
        let sources = vec![
            PostgresSource {
                id: "main_runtime".to_string(),
                instance: "main".to_string(),
                database_oid: 42,
                role: PostgresSourceRole::Runtime,
                dsn: "postgres://runtime".to_string(),
            },
            PostgresSource {
                id: "main_datawarehouse".to_string(),
                instance: "main".to_string(),
                database_oid: 84,
                role: PostgresSourceRole::Datawarehouse,
                dsn: "postgres://dw".to_string(),
            },
        ];

        let rendered = render_dbhub_toml(&sources, 30, 1000);

        assert!(rendered.contains("id = \"main_runtime\""));
        assert!(rendered.contains("# database_oid = 42"));
        assert!(rendered.contains("source = \"main_runtime\""));
        assert!(rendered.contains("id = \"main_datawarehouse\""));
        assert!(rendered.contains("# database_oid = 84"));
        assert!(rendered.contains("source = \"main_datawarehouse\""));
        assert!(rendered.contains("readonly = true"));
        assert!(rendered.contains("max_rows = 1000"));
    }
}
