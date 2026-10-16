//! Per-instance Lana admin MCP upstreams.
//!
//! Every Lana instance (sandbox or long-lived) serves a read-only admin MCP
//! endpoint (`list_data_mart_models`, `describe_data_mart_model`,
//! `run_readonly_dw_query`) on its admin server, gated by the instance's
//! Keycloak realm (`<instance>-internal`) with the `lana-admin-mcp` scope.
//!
//! This controller discovers Ready `LanaSandbox` CRs and maps each one — plus
//! any statically configured instances — to an authenticated upstream. The
//! connector mints tokens via the realm's `admin-mcp-gateway` direct-grant
//! client (see lana-bank `tf/modules/keycloak-realms`), which only exists
//! where DEV auth flows are enabled (staging sandboxes, kind).

use async_trait::async_trait;
use kube::{
    api::ListParams,
    core::{ApiResource, DynamicObject, GroupVersionKind},
    Api, Client,
};

use crate::mcp_upstream::{DirectGrantAuth, UpstreamConfig};

pub(crate) const DEFAULT_LANA_ADMIN_MCP_SANDBOX_NAMESPACE: &str = "lana-sandbox";
pub(crate) const DEFAULT_LANA_ADMIN_MCP_CLIENT_ID: &str = "admin-mcp-gateway";
pub(crate) const DEFAULT_LANA_ADMIN_MCP_USERNAME_TEMPLATE: &str =
    "{instance}-superuser@mailinator.com";
pub(crate) const DEFAULT_LANA_ADMIN_MCP_URL_TEMPLATE: &str =
    "http://lana-bank-admin.{instance}.svc.cluster.local:5253/mcp";
pub(crate) const LANA_ADMIN_MCP_UPSTREAM_PREFIX: &str = "lana_admin";

const LANA_SANDBOX_GROUP: &str = "sandbox.galoy.io";
const LANA_SANDBOX_VERSION: &str = "v1alpha1";
const LANA_SANDBOX_KIND: &str = "LanaSandbox";

/// A non-sandbox Lana instance (e.g. staging `main`) exposed by explicit
/// name + MCP URL; the realm/username conventions still apply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StaticLanaAdminInstance {
    pub(crate) name: String,
    pub(crate) url: String,
}

#[derive(Clone, Debug)]
pub(crate) struct LanaAdminMcpConfig {
    /// Namespace holding the LanaSandbox CRs.
    pub(crate) sandbox_namespace: String,
    /// Public Keycloak base URL, e.g. `https://auth.staging.galoy.io`.
    pub(crate) keycloak_base_url: String,
    /// Direct-grant client id minting `lana-admin-mcp` tokens.
    pub(crate) client_id: String,
    /// Username template; `{instance}` is replaced with the instance name.
    pub(crate) username_template: String,
    /// Password for the direct grant. Empty under DEV auth flows.
    pub(crate) password: String,
    /// Admin MCP URL template for discovered sandboxes; `{instance}` is
    /// replaced with the sandbox (and namespace) name.
    pub(crate) url_template: String,
    /// Explicitly configured instances that are not LanaSandbox CRs.
    pub(crate) static_instances: Vec<StaticLanaAdminInstance>,
}

/// Outbound discovery port — lists Ready sandbox instance names.
#[async_trait]
pub(crate) trait LanaAdminMcpInstanceDiscoverer: Clone + Send + Sync + 'static {
    async fn discover_ready_sandboxes(
        &self,
        config: &LanaAdminMcpConfig,
    ) -> anyhow::Result<Vec<String>>;
}

#[derive(Clone)]
pub(crate) struct KubernetesLanaAdminMcpDiscoverer {
    client: Client,
}

impl KubernetesLanaAdminMcpDiscoverer {
    pub(crate) async fn try_default() -> anyhow::Result<Self> {
        Ok(Self {
            client: Client::try_default().await?,
        })
    }
}

#[async_trait]
impl LanaAdminMcpInstanceDiscoverer for KubernetesLanaAdminMcpDiscoverer {
    async fn discover_ready_sandboxes(
        &self,
        config: &LanaAdminMcpConfig,
    ) -> anyhow::Result<Vec<String>> {
        let resource = ApiResource::from_gvk(&GroupVersionKind::gvk(
            LANA_SANDBOX_GROUP,
            LANA_SANDBOX_VERSION,
            LANA_SANDBOX_KIND,
        ));
        let api: Api<DynamicObject> =
            Api::namespaced_with(self.client.clone(), &config.sandbox_namespace, &resource);
        let sandboxes = api.list(&ListParams::default()).await?;

        Ok(sandboxes
            .items
            .into_iter()
            .filter(|sandbox| sandbox.metadata.deletion_timestamp.is_none())
            .filter(|sandbox| {
                sandbox
                    .data
                    .get("status")
                    .and_then(|status| status.get("phase"))
                    .and_then(|phase| phase.as_str())
                    == Some("Ready")
            })
            .filter_map(|sandbox| sandbox.metadata.name)
            .collect())
    }
}

#[derive(Clone)]
pub(crate) struct LanaAdminMcpController<D> {
    discoverer: D,
    config: LanaAdminMcpConfig,
}

impl<D> LanaAdminMcpController<D>
where
    D: LanaAdminMcpInstanceDiscoverer,
{
    pub(crate) fn try_new(config: LanaAdminMcpConfig, discoverer: D) -> anyhow::Result<Self> {
        config.validate()?;

        Ok(Self { discoverer, config })
    }

    /// Resolve the current set of lana-admin MCP upstreams: one per Ready
    /// sandbox plus every static instance. Sandboxes mid-teardown or not yet
    /// Ready simply drop out; the tunnel session's upstream-set comparison
    /// triggers a re-registration when the set changes.
    pub(crate) async fn reconcile(&self) -> anyhow::Result<Vec<UpstreamConfig>> {
        let mut instances = self
            .discoverer
            .discover_ready_sandboxes(&self.config)
            .await?;
        instances.sort();

        let mut upstreams =
            Vec::with_capacity(instances.len() + self.config.static_instances.len());
        for instance in instances {
            match self.upstream_for_instance(&instance, None) {
                Some(upstream) => upstreams.push(upstream),
                None => tracing::warn!(
                    instance = %instance,
                    "skipping lana admin mcp sandbox with unusable name"
                ),
            }
        }
        for instance in &self.config.static_instances {
            match self.upstream_for_instance(&instance.name, Some(&instance.url)) {
                Some(upstream) => upstreams.push(upstream),
                None => tracing::warn!(
                    instance = %instance.name,
                    "skipping static lana admin mcp instance with unusable name"
                ),
            }
        }

        Ok(upstreams)
    }

    fn upstream_for_instance(&self, instance: &str, url: Option<&str>) -> Option<UpstreamConfig> {
        let normalized = normalize_upstream_component(instance)?;
        let url = match url {
            Some(url) => url.to_string(),
            None => self.config.url_template.replace("{instance}", instance),
        };

        Some(UpstreamConfig {
            name: format!("{LANA_ADMIN_MCP_UPSTREAM_PREFIX}_{normalized}"),
            url,
            auth: Some(DirectGrantAuth {
                token_url: format!(
                    "{}/realms/{instance}-internal/protocol/openid-connect/token",
                    self.config.keycloak_base_url.trim_end_matches('/')
                ),
                client_id: self.config.client_id.clone(),
                username: self
                    .config
                    .username_template
                    .replace("{instance}", instance),
                password: self.config.password.clone(),
            }),
        })
    }
}

impl LanaAdminMcpConfig {
    fn validate(&self) -> anyhow::Result<()> {
        if self.keycloak_base_url.trim().is_empty() {
            anyhow::bail!("lana admin mcp keycloak base url must not be empty");
        }
        if self.sandbox_namespace.trim().is_empty() {
            anyhow::bail!("lana admin mcp sandbox namespace must not be empty");
        }
        for (field, template) in [
            ("username_template", &self.username_template),
            ("url_template", &self.url_template),
        ] {
            if !template.contains("{instance}") {
                anyhow::bail!("lana admin mcp {field} must contain an {{instance}} placeholder");
            }
        }
        for instance in &self.static_instances {
            if instance.name.trim().is_empty() || instance.url.trim().is_empty() {
                anyhow::bail!("lana admin mcp static instances need non-empty name and url");
            }
        }

        Ok(())
    }
}

/// Parse `TUNNEL_LANA_ADMIN_MCP_STATIC_INSTANCES`: `name=url[,name=url,...]`.
pub(crate) fn parse_static_instances(raw: &str) -> Vec<StaticLanaAdminInstance> {
    raw.split(',')
        .filter_map(|pair| {
            let (name, url) = pair.split_once('=')?;
            Some(StaticLanaAdminInstance {
                name: name.trim().to_string(),
                url: url.trim().to_string(),
            })
        })
        .filter(|instance| !instance.name.is_empty() && !instance.url.is_empty())
        .collect()
}

/// Same alphabet as the postgres source ids: drua prefixes tools with the
/// upstream name, so keep it a valid lowercase identifier.
fn normalize_upstream_component(value: &str) -> Option<String> {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> LanaAdminMcpConfig {
        LanaAdminMcpConfig {
            sandbox_namespace: "lana-sandbox".to_string(),
            keycloak_base_url: "https://auth.staging.galoy.io".to_string(),
            client_id: DEFAULT_LANA_ADMIN_MCP_CLIENT_ID.to_string(),
            username_template: DEFAULT_LANA_ADMIN_MCP_USERNAME_TEMPLATE.to_string(),
            password: String::new(),
            url_template: DEFAULT_LANA_ADMIN_MCP_URL_TEMPLATE.to_string(),
            static_instances: vec![StaticLanaAdminInstance {
                name: "main".to_string(),
                url: "http://lana-bank-admin.lana-bank-main.svc.cluster.local:5253/mcp".to_string(),
            }],
        }
    }

    #[derive(Clone)]
    struct FakeDiscoverer {
        instances: Vec<String>,
    }

    #[async_trait]
    impl LanaAdminMcpInstanceDiscoverer for FakeDiscoverer {
        async fn discover_ready_sandboxes(
            &self,
            _config: &LanaAdminMcpConfig,
        ) -> anyhow::Result<Vec<String>> {
            Ok(self.instances.clone())
        }
    }

    #[tokio::test]
    async fn reconcile_maps_sandboxes_and_static_instances_to_authenticated_upstreams(
    ) -> anyhow::Result<()> {
        let controller = LanaAdminMcpController::try_new(
            test_config(),
            FakeDiscoverer {
                instances: vec!["sb-demo-verify".to_string()],
            },
        )?;

        let upstreams = controller.reconcile().await?;

        assert_eq!(upstreams.len(), 2);

        let sandbox = &upstreams[0];
        assert_eq!(sandbox.name, "lana_admin_sb_demo_verify");
        assert_eq!(
            sandbox.url,
            "http://lana-bank-admin.sb-demo-verify.svc.cluster.local:5253/mcp"
        );
        let auth = sandbox.auth.as_ref().expect("sandbox upstream has auth");
        assert_eq!(
            auth.token_url,
            "https://auth.staging.galoy.io/realms/sb-demo-verify-internal/protocol/openid-connect/token"
        );
        assert_eq!(auth.client_id, "admin-mcp-gateway");
        assert_eq!(auth.username, "sb-demo-verify-superuser@mailinator.com");

        let static_instance = &upstreams[1];
        assert_eq!(static_instance.name, "lana_admin_main");
        assert_eq!(
            static_instance.url,
            "http://lana-bank-admin.lana-bank-main.svc.cluster.local:5253/mcp"
        );
        let auth = static_instance
            .auth
            .as_ref()
            .expect("static upstream has auth");
        assert_eq!(
            auth.token_url,
            "https://auth.staging.galoy.io/realms/main-internal/protocol/openid-connect/token"
        );

        Ok(())
    }

    #[tokio::test]
    async fn reconcile_drops_sandboxes_that_disappear() -> anyhow::Result<()> {
        let mut config = test_config();
        config.static_instances = Vec::new();

        let full = LanaAdminMcpController::try_new(
            config.clone(),
            FakeDiscoverer {
                instances: vec!["sb-a".to_string(), "sb-b".to_string()],
            },
        )?;
        let torn_down = LanaAdminMcpController::try_new(
            config,
            FakeDiscoverer {
                instances: vec!["sb-a".to_string()],
            },
        )?;

        assert_eq!(full.reconcile().await?.len(), 2);
        assert_eq!(torn_down.reconcile().await?.len(), 1);

        Ok(())
    }

    #[test]
    fn static_instances_parse_name_url_pairs() {
        let parsed = parse_static_instances(
            "main=http://lana-bank-admin.lana-bank-main.svc:5253/mcp, canary=https://admin.canary.staging.galoy.io/mcp",
        );

        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].name, "main");
        assert_eq!(parsed[1].url, "https://admin.canary.staging.galoy.io/mcp");
    }

    #[test]
    fn config_requires_instance_placeholder_in_templates() {
        let mut config = test_config();
        config.url_template = "http://fixed-url:5253/mcp".to_string();

        let result = LanaAdminMcpController::try_new(
            config,
            FakeDiscoverer {
                instances: Vec::new(),
            },
        );

        assert!(result.is_err());
    }
}
