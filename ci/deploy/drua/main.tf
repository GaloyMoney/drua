variable "image_digest" {}
variable "sandbox_image_digest" {}
variable "github_client_secret" {}
variable "concourse_username" {}
variable "concourse_password" {}
variable "lana_bank_cachix_enabled" {
  default = false
}
variable "lana_bank_cachix_auth_token" {
  default   = ""
  sensitive = true
}
variable "lana_bank_cachix_public_key" {
  default = ""
}
variable "honeycomb_api_key" {
  default = ""
}
variable "github_pat" {
  default = ""
}
variable "anthropic_api_key" {
  default = ""
}
variable "openrouter_api_key" {
  default = ""
}
variable "zenduty_api_token" {
  default = ""
}
locals {
  cluster_name         = "galoy-agents-cluster"
  cluster_location     = "us-east1-b"
  gcp_project          = "galoy-agents"
  namespace            = "galoy-agents"
  sandbox_namespace    = "galoy-agents-sandboxes"
  controller_namespace = "galoy-agents-sandbox-controller"
  vpc_name             = "galoy-agents-vpc"
  region               = "us-east1"

  github_app_private_key = fileexists("${path.module}/github-app-private-key.pem") ? file("${path.module}/github-app-private-key.pem") : ""

  tunnel_deployments = fileexists("${path.module}/tunnel-deployments-public-keys.json") ? jsondecode(file("${path.module}/tunnel-deployments-public-keys.json")) : {}

  # The helm provider diffs `helm_release` on chart name + version + values,
  # NOT on the chart's file contents. With `chart = "./chart"` and
  # `Chart.yaml` pinned to a static version, edits to the bundled chart
  # (e.g. `values.yaml` provider list) silently fail to upgrade. Hashing
  # every file under ./chart and feeding the digest in via a synthetic
  # `set` makes any chart edit produce a `values` diff and force an
  # upgrade.
  chart_files_hash = sha256(join("", [
    for f in sort(fileset("${path.module}/chart", "**")) :
    filesha256("${path.module}/chart/${f}")
  ]))
}

module "postgresql" {
  source = "git::https://github.com/GaloyMoney/galoy-infra.git//modules/postgresql/gcp?ref=main"

  gcp_project      = local.gcp_project
  vpc_name         = local.vpc_name
  instance_name    = "galoy-agents"
  region           = local.region
  databases        = ["galoy-agents"]
  destroyable      = true
  highly_available = false
  tier             = "db-f1-micro"
  replication      = false
  readonly_users   = ["mcp"]
}

resource "kubernetes_namespace" "galoy_agents" {
  metadata {
    name = local.namespace
  }
}

resource "kubernetes_namespace" "sandbox" {
  metadata {
    name = local.sandbox_namespace
  }
}

resource "kubernetes_namespace" "sandbox_controller" {
  metadata {
    name = local.controller_namespace
  }
}

resource "kubernetes_secret" "galoy_agents" {
  metadata {
    name      = "galoy-agents"
    namespace = local.namespace
  }

  data = {
    "pg-con" = module.postgresql.creds["galoy-agents"].conn
    # Read-only DSN for the postgres-mcp sidecar. `?sslmode=require` is
    # required — Cloud SQL's pg_hba.conf rejects unencrypted connections
    # and dbhub crash-loops without it.
    "pg-mcp-uri"                 = "postgres://${module.postgresql.creds["galoy-agents"].readonly_users["mcp"].user}:${module.postgresql.creds["galoy-agents"].readonly_users["mcp"].password}@${module.postgresql.creds["galoy-agents"].host}:5432/galoy-agents?sslmode=require"
    "github-client-secret"       = var.github_client_secret
    "gcs-creds"                  = file("${path.module}/gcs-creds.json")
    "concourse-username"         = var.concourse_username
    "concourse-password"         = var.concourse_password
    "honeycomb-auth-header"               = var.honeycomb_api_key != "" ? "Bearer ${var.honeycomb_api_key}" : ""
    "github-auth-header"                  = var.github_pat != "" ? "Bearer ${var.github_pat}" : ""
    "github_actions-auth-header"          = var.github_pat != "" ? "Bearer ${var.github_pat}" : ""
    "github_pull_requests-auth-header"    = var.github_pat != "" ? "Bearer ${var.github_pat}" : ""
    "anthropic-api-key"          = var.anthropic_api_key
    "openai-api-key"             = var.openrouter_api_key
    "zenduty-api-token"          = var.zenduty_api_token
    "github-app-private-key"     = local.github_app_private_key
  }

  depends_on = [kubernetes_namespace.galoy_agents]
}

resource "kubernetes_secret" "sandbox_nix_netrc" {
  count = var.lana_bank_cachix_enabled ? 1 : 0

  metadata {
    name      = "sandbox-nix-netrc"
    namespace = local.sandbox_namespace
  }

  data = {
    netrc = "machine lana-bank-github-actions.cachix.org password ${var.lana_bank_cachix_auth_token}\n"
  }

  depends_on = [kubernetes_namespace.sandbox]
}

resource "google_container_node_pool" "gvisor" {
  provider = google-beta
  project  = local.gcp_project
  cluster  = local.cluster_name
  location = local.cluster_location

  name       = "sandbox-gvisor"
  node_count = 0

  autoscaling {
    min_node_count = 0
    max_node_count = 2
  }

  node_config {
    # 8 vCPU / 32 GiB to fit a 4 vCPU / 8 GiB sandbox (e.g. full lana-bank
    # dev stack) with headroom for system daemons and gvisor overhead
    # (~10-15%). Was e2-standard-4, but a 4-CPU pod cannot schedule on a
    # 4-CPU node — system daemonsets reserve ~80m before any workload.
    machine_type = "e2-standard-8"

    sandbox_config {
      sandbox_type = "gvisor"
    }

    oauth_scopes = [
      "https://www.googleapis.com/auth/cloud-platform",
    ]
  }

  management {
    auto_repair  = true
    auto_upgrade = true
  }

  lifecycle {
    ignore_changes = [node_count]
  }
}

# ---------------------------------------------------------------------------
# Agent Sandbox controller + CRDs
# Installed separately so the CRDs are registered before Helm tries to
# create SandboxTemplate / SandboxWarmPool custom resources.
# ---------------------------------------------------------------------------
data "kubectl_file_documents" "sandbox_controller" {
  content = replace(
    file("${path.module}/chart/vendor/agent-sandbox/manifest.yaml"),
    "agent-sandbox-system",
    local.controller_namespace
  )
}

resource "kubectl_manifest" "sandbox_controller" {
  for_each  = data.kubectl_file_documents.sandbox_controller.manifests
  yaml_body = each.value

  depends_on = [
    google_container_node_pool.gvisor,
    kubernetes_namespace.sandbox_controller,
  ]
}

data "kubectl_file_documents" "sandbox_extensions" {
  content = replace(
    file("${path.module}/chart/vendor/agent-sandbox/extensions.yaml"),
    "agent-sandbox-system",
    local.controller_namespace
  )
}

resource "kubectl_manifest" "sandbox_extensions" {
  for_each  = data.kubectl_file_documents.sandbox_extensions.manifests
  yaml_body = each.value

  depends_on = [
    google_container_node_pool.gvisor,
    kubernetes_namespace.sandbox_controller,
  ]
}

resource "postgresql_extension" "vector" {
  provider = postgresql.galoy_agents
  name     = "vector"
  database = "galoy-agents"

  depends_on = [module.postgresql]
}

resource "helm_release" "galoy_agents" {
  name      = "galoy-agents"
  chart     = "${path.module}/chart"
  namespace = local.namespace

  values = [
    templatefile("${path.module}/prod-values.yml.tmpl", {
      image_digest                = var.image_digest
      sandbox_image_digest        = var.sandbox_image_digest
      secret_checksum             = sha256(jsonencode(kubernetes_secret.galoy_agents.data))
      tunnel_deployments          = local.tunnel_deployments
      lana_bank_cachix_enabled    = var.lana_bank_cachix_enabled
      lana_bank_cachix_public_key = var.lana_bank_cachix_public_key
    })
  ]

  # Forces a `values` diff whenever any file under ./chart changes — see
  # local.chart_files_hash. The chart itself ignores the value.
  set {
    name  = "chartFilesHash"
    value = local.chart_files_hash
  }

  dependency_update = true
  timeout           = 900 # 15 minutes

  depends_on = [
    kubernetes_secret.galoy_agents,
    kubernetes_secret.sandbox_nix_netrc,
    kubernetes_namespace.sandbox,
    google_container_node_pool.gvisor,
    kubectl_manifest.sandbox_controller,
    kubectl_manifest.sandbox_extensions,
    postgresql_extension.vector,
  ]
}

data "google_container_cluster" "primary" {
  project  = local.gcp_project
  name     = local.cluster_name
  location = local.cluster_location
}

data "google_client_config" "default" {
  provider = google-beta
}

provider "kubernetes" {
  host                   = "https://${data.google_container_cluster.primary.private_cluster_config.0.private_endpoint}"
  token                  = data.google_client_config.default.access_token
  cluster_ca_certificate = base64decode(data.google_container_cluster.primary.master_auth.0.cluster_ca_certificate)
}

provider "kubectl" {
  host                   = "https://${data.google_container_cluster.primary.private_cluster_config.0.private_endpoint}"
  token                  = data.google_client_config.default.access_token
  cluster_ca_certificate = base64decode(data.google_container_cluster.primary.master_auth.0.cluster_ca_certificate)
  load_config_file       = false
}

provider "postgresql" {
  alias     = "galoy_agents"
  host      = module.postgresql.creds["galoy-agents"].host
  port      = 5432
  username  = module.postgresql.admin-creds.user
  password  = module.postgresql.admin-creds.password
  database  = "galoy-agents"
  superuser = false
}

provider "helm" {
  kubernetes {
    host                   = "https://${data.google_container_cluster.primary.private_cluster_config.0.private_endpoint}"
    token                  = data.google_client_config.default.access_token
    cluster_ca_certificate = base64decode(data.google_container_cluster.primary.master_auth.0.cluster_ca_certificate)
  }
}
