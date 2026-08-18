use clap::Parser;

use crate::lana_admin_mcp::{
    DEFAULT_LANA_ADMIN_MCP_CLIENT_ID, DEFAULT_LANA_ADMIN_MCP_SANDBOX_NAMESPACE,
    DEFAULT_LANA_ADMIN_MCP_URL_TEMPLATE, DEFAULT_LANA_ADMIN_MCP_USERNAME_TEMPLATE,
};
use crate::postgres_mcp::{
    DEFAULT_POSTGRES_MCP_CLOUD_SQL_PROXY_IMAGE, DEFAULT_POSTGRES_MCP_CONFIG_SECRET,
    DEFAULT_POSTGRES_MCP_CONNECT_TIMEOUT_SECS, DEFAULT_POSTGRES_MCP_IMAGE,
    DEFAULT_POSTGRES_MCP_LIMIT_CPU, DEFAULT_POSTGRES_MCP_LIMIT_MEMORY,
    DEFAULT_POSTGRES_MCP_MAX_ROWS, DEFAULT_POSTGRES_MCP_QUERY_TIMEOUT,
    DEFAULT_POSTGRES_MCP_REQUEST_CPU, DEFAULT_POSTGRES_MCP_REQUEST_MEMORY,
    DEFAULT_POSTGRES_MCP_RESOURCE_NAME, DEFAULT_POSTGRES_MCP_SERVICE_PORT,
    DEFAULT_POSTGRES_MCP_UPSTREAM_NAME,
};

pub(crate) const DEFAULT_TOOL_REFRESH_INTERVAL_SECS: u64 = 30;

#[derive(Parser)]
#[command(
    name = "tunnel-connector",
    about = "Outbound tunnel from a deployment cluster to drua"
)]
pub(crate) struct Cli {
    /// drua tunnel WebSocket URL
    #[arg(long, env = "TUNNEL_SERVER_URL")]
    pub(crate) server_url: String,

    /// Path to the PEM-encoded Ed25519 private key that identifies this
    /// deployment. The matching public key must be registered on drua
    /// under `server.tunnel.deployments.<deployment_id>`. Mounted from
    /// a Kubernetes Secret in production.
    #[arg(long, env = "TUNNEL_PRIVATE_KEY_FILE")]
    pub(crate) private_key_file: std::path::PathBuf,

    /// Deployment identifier (e.g. "galoy-staging"). Must match the
    /// key under `server.tunnel.deployments` in drua's config.
    #[arg(long, env = "TUNNEL_DEPLOYMENT_ID")]
    pub(crate) deployment_id: String,

    /// Comma-separated upstream MCP servers: name=url[,name=url,...].
    #[arg(long, env = "TUNNEL_UPSTREAMS", default_value = "")]
    pub(crate) upstreams: String,

    /// Poll interval for upstream MCP tool catalog changes. A value of 0 disables refresh.
    #[arg(
        long,
        env = "TUNNEL_TOOL_REFRESH_INTERVAL_SECS",
        default_value_t = DEFAULT_TOOL_REFRESH_INTERVAL_SECS
    )]
    pub(crate) tool_refresh_interval_secs: u64,

    /// Namespace containing the generated DBHub resources. Defaults to the pod namespace.
    #[arg(long, env = "TUNNEL_POSTGRES_MCP_NAMESPACE")]
    pub(crate) postgres_mcp_namespace: Option<String>,

    /// Seed readonly DSN for the Lana runtime PostgreSQL instance.
    #[arg(long, env = "TUNNEL_POSTGRES_MCP_RUNTIME_DSN")]
    pub(crate) postgres_mcp_runtime_dsn: String,

    /// Optional seed readonly DSN for the Lana datawarehouse PostgreSQL instance.
    #[arg(long, env = "TUNNEL_POSTGRES_MCP_DATAWAREHOUSE_DSN")]
    pub(crate) postgres_mcp_datawarehouse_dsn: Option<String>,

    /// Optional service account name for the generated aggregate DBHub pods.
    /// Required when DBHub needs Workload Identity for Cloud SQL proxy sidecars.
    #[arg(long, env = "TUNNEL_POSTGRES_MCP_SERVICE_ACCOUNT_NAME")]
    pub(crate) postgres_mcp_service_account_name: Option<String>,

    /// Fixed name for the generated aggregate DBHub Deployment and Service.
    #[arg(
        long,
        env = "TUNNEL_POSTGRES_MCP_RESOURCE_NAME",
        default_value = DEFAULT_POSTGRES_MCP_RESOURCE_NAME
    )]
    pub(crate) postgres_mcp_resource_name: String,

    /// Fixed name for the generated DBHub config Secret.
    #[arg(
        long,
        env = "TUNNEL_POSTGRES_MCP_CONFIG_SECRET",
        default_value = DEFAULT_POSTGRES_MCP_CONFIG_SECRET
    )]
    pub(crate) postgres_mcp_config_secret: String,

    /// Drua upstream name used for the aggregate DBHub MCP service.
    #[arg(
        long,
        env = "TUNNEL_POSTGRES_MCP_UPSTREAM_NAME",
        default_value = DEFAULT_POSTGRES_MCP_UPSTREAM_NAME
    )]
    pub(crate) postgres_mcp_upstream_name: String,

    /// DBHub image used for the aggregate Postgres MCP server.
    #[arg(
        long,
        env = "TUNNEL_POSTGRES_MCP_IMAGE",
        default_value = DEFAULT_POSTGRES_MCP_IMAGE
    )]
    pub(crate) postgres_mcp_image: String,

    /// Image pull policy for the DBHub container.
    #[arg(
        long,
        env = "TUNNEL_POSTGRES_MCP_IMAGE_PULL_POLICY",
        default_value = "IfNotPresent"
    )]
    pub(crate) postgres_mcp_image_pull_policy: String,

    /// Cloud SQL Auth Proxy image used by generated DBHub pods when proxy args are configured.
    #[arg(
        long,
        env = "TUNNEL_POSTGRES_MCP_CLOUD_SQL_PROXY_IMAGE",
        default_value = DEFAULT_POSTGRES_MCP_CLOUD_SQL_PROXY_IMAGE
    )]
    pub(crate) postgres_mcp_cloud_sql_proxy_image: String,

    /// Image pull policy for generated DBHub Cloud SQL proxy sidecars.
    #[arg(
        long,
        env = "TUNNEL_POSTGRES_MCP_CLOUD_SQL_PROXY_IMAGE_PULL_POLICY",
        default_value = "IfNotPresent"
    )]
    pub(crate) postgres_mcp_cloud_sql_proxy_image_pull_policy: String,

    /// Optional Cloud SQL Auth Proxy instance arg for the Lana runtime instance,
    /// for example `project:region:instance?port=5432&private-ip=true`.
    #[arg(long, env = "TUNNEL_POSTGRES_MCP_RUNTIME_CLOUD_SQL_PROXY_ARG")]
    pub(crate) postgres_mcp_runtime_cloud_sql_proxy_arg: Option<String>,

    /// Optional Cloud SQL Auth Proxy instance arg for the Lana datawarehouse instance,
    /// for example `project:region:instance?port=5433&private-ip=true`.
    #[arg(long, env = "TUNNEL_POSTGRES_MCP_DATAWAREHOUSE_CLOUD_SQL_PROXY_ARG")]
    pub(crate) postgres_mcp_datawarehouse_cloud_sql_proxy_arg: Option<String>,

    /// DBHub HTTP port.
    #[arg(
        long,
        env = "TUNNEL_POSTGRES_MCP_SERVICE_PORT",
        default_value_t = DEFAULT_POSTGRES_MCP_SERVICE_PORT
    )]
    pub(crate) postgres_mcp_service_port: u16,

    /// DBHub query timeout, in seconds, rendered into each source.
    #[arg(
        long,
        env = "TUNNEL_POSTGRES_MCP_QUERY_TIMEOUT",
        default_value_t = DEFAULT_POSTGRES_MCP_QUERY_TIMEOUT
    )]
    pub(crate) postgres_mcp_query_timeout: u32,

    /// DBHub maximum rows returned by execute_sql.
    #[arg(
        long,
        env = "TUNNEL_POSTGRES_MCP_MAX_ROWS",
        default_value_t = DEFAULT_POSTGRES_MCP_MAX_ROWS
    )]
    pub(crate) postgres_mcp_max_rows: u32,

    /// Timeout for Postgres discovery and validation checks.
    #[arg(
        long,
        env = "TUNNEL_POSTGRES_MCP_CONNECT_TIMEOUT_SECS",
        default_value_t = DEFAULT_POSTGRES_MCP_CONNECT_TIMEOUT_SECS
    )]
    pub(crate) postgres_mcp_connect_timeout_secs: u64,

    /// CPU request for generated DBHub pods.
    #[arg(
        long,
        env = "TUNNEL_POSTGRES_MCP_REQUEST_CPU",
        default_value = DEFAULT_POSTGRES_MCP_REQUEST_CPU
    )]
    pub(crate) postgres_mcp_request_cpu: String,

    /// Memory request for generated DBHub pods.
    #[arg(
        long,
        env = "TUNNEL_POSTGRES_MCP_REQUEST_MEMORY",
        default_value = DEFAULT_POSTGRES_MCP_REQUEST_MEMORY
    )]
    pub(crate) postgres_mcp_request_memory: String,

    /// Enable per-instance Lana admin MCP upstreams: Ready LanaSandbox CRs are
    /// discovered automatically, static instances come from
    /// TUNNEL_LANA_ADMIN_MCP_STATIC_INSTANCES.
    #[arg(long, env = "TUNNEL_LANA_ADMIN_MCP_ENABLED", default_value_t = false)]
    pub(crate) lana_admin_mcp_enabled: bool,

    /// Namespace holding the LanaSandbox CRs.
    #[arg(
        long,
        env = "TUNNEL_LANA_ADMIN_MCP_SANDBOX_NAMESPACE",
        default_value = DEFAULT_LANA_ADMIN_MCP_SANDBOX_NAMESPACE
    )]
    pub(crate) lana_admin_mcp_sandbox_namespace: String,

    /// Public Keycloak base URL the per-instance realms live on, e.g.
    /// https://auth.staging.galoy.io.
    #[arg(long, env = "TUNNEL_LANA_ADMIN_MCP_KEYCLOAK_BASE_URL")]
    pub(crate) lana_admin_mcp_keycloak_base_url: Option<String>,

    /// Direct-grant client id that mints lana-admin-mcp tokens.
    #[arg(
        long,
        env = "TUNNEL_LANA_ADMIN_MCP_CLIENT_ID",
        default_value = DEFAULT_LANA_ADMIN_MCP_CLIENT_ID
    )]
    pub(crate) lana_admin_mcp_client_id: String,

    /// Username template for the direct grant; `{instance}` is replaced with
    /// the instance name.
    #[arg(
        long,
        env = "TUNNEL_LANA_ADMIN_MCP_USERNAME_TEMPLATE",
        default_value = DEFAULT_LANA_ADMIN_MCP_USERNAME_TEMPLATE
    )]
    pub(crate) lana_admin_mcp_username_template: String,

    /// Password for the direct grant. Empty where the realm binds the DEV
    /// direct-grant flow (staging sandboxes, kind).
    #[arg(long, env = "TUNNEL_LANA_ADMIN_MCP_PASSWORD", default_value = "")]
    pub(crate) lana_admin_mcp_password: String,

    /// Admin MCP URL template for discovered sandboxes; `{instance}` is
    /// replaced with the sandbox (and namespace) name.
    #[arg(
        long,
        env = "TUNNEL_LANA_ADMIN_MCP_URL_TEMPLATE",
        default_value = DEFAULT_LANA_ADMIN_MCP_URL_TEMPLATE
    )]
    pub(crate) lana_admin_mcp_url_template: String,

    /// Comma-separated name=url pairs for Lana instances that are not
    /// LanaSandbox CRs (e.g. main=http://lana-bank-admin.lana-bank-main.svc:5253/mcp).
    #[arg(
        long,
        env = "TUNNEL_LANA_ADMIN_MCP_STATIC_INSTANCES",
        default_value = ""
    )]
    pub(crate) lana_admin_mcp_static_instances: String,

    /// CPU limit for generated DBHub pods.
    #[arg(
        long,
        env = "TUNNEL_POSTGRES_MCP_LIMIT_CPU",
        default_value = DEFAULT_POSTGRES_MCP_LIMIT_CPU
    )]
    pub(crate) postgres_mcp_limit_cpu: String,

    /// Memory limit for generated DBHub pods.
    #[arg(
        long,
        env = "TUNNEL_POSTGRES_MCP_LIMIT_MEMORY",
        default_value = DEFAULT_POSTGRES_MCP_LIMIT_MEMORY
    )]
    pub(crate) postgres_mcp_limit_memory: String,
}
