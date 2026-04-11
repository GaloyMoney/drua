variable "image_digest" {}
variable "sandbox_base_image_digest" {}
variable "github_client_secret" {}
variable "concourse_username" {}
variable "concourse_password" {}
variable "honeycomb_api_key" {
  default = ""
}
variable "github_pat" {
  default = ""
}
variable "lingo_api_key" {
  default = ""
}
variable "anthropic_api_key" {
  default = ""
}
variable "github_app_private_key" {
  default   = ""
  sensitive = true
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
}

module "postgresql" {
  source = "git::https://github.com/GaloyMoney/galoy-infra.git//modules/postgresql/gcp?ref=main"

  gcp_project    = local.gcp_project
  vpc_name       = local.vpc_name
  instance_name  = "galoy-agents"
  region         = local.region
  databases      = ["galoy-agents"]
  destroyable    = true
  highly_available = false
  tier           = "db-f1-micro"
  replication    = false
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
    "pg-con"               = module.postgresql.creds["galoy-agents"].conn
    "github-client-secret" = var.github_client_secret
    "gcs-creds"            = file("${path.module}/gcs-creds.json")
    "concourse-username"   = var.concourse_username
    "concourse-password"   = var.concourse_password
    "honeycomb-auth-header" = var.honeycomb_api_key != "" ? "Bearer ${var.honeycomb_api_key}" : ""
    "github-auth-header"   = var.github_pat != "" ? "Bearer ${var.github_pat}" : ""
    "lingo-auth-header"    = var.lingo_api_key
    "anthropic-api-key"    = var.anthropic_api_key
    "github-app-private-key" = var.github_app_private_key
  }

  depends_on = [kubernetes_namespace.galoy_agents]
}

resource "kubernetes_secret" "sandbox_anthropic" {
  metadata {
    name      = "anthropic-api-key"
    namespace = local.sandbox_namespace
  }

  data = {
    "api-key" = var.anthropic_api_key
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
    max_node_count = 1
  }

  node_config {
    machine_type = "e2-standard-2"

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
      sandbox_base_image_digest   = var.sandbox_base_image_digest
      secret_checksum             = sha256(jsonencode(kubernetes_secret.galoy_agents.data))
    })
  ]

  dependency_update = true
  force_update      = true
  timeout           = 900 # 15 minutes

  depends_on = [
    kubernetes_secret.galoy_agents,
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
  alias    = "galoy_agents"
  host     = module.postgresql.creds["galoy-agents"].host
  port     = 5432
  username = module.postgresql.admin-creds.user
  password = module.postgresql.admin-creds.password
  database = "galoy-agents"
  superuser = false
}

provider "helm" {
  kubernetes {
    host                   = "https://${data.google_container_cluster.primary.private_cluster_config.0.private_endpoint}"
    token                  = data.google_client_config.default.access_token
    cluster_ca_certificate = base64decode(data.google_container_cluster.primary.master_auth.0.cluster_ca_certificate)
  }
}
