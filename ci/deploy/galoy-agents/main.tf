variable "image_digest" {}
variable "sandbox_base_image_digest" {}
variable "github_client_secret" {}
variable "concourse_username" {}
variable "concourse_password" {}
variable "honeycomb_api_key" {
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
  }

  depends_on = [kubernetes_namespace.galoy_agents]
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
    max_node_count = 3
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

resource "helm_release" "galoy_agents" {
  name      = "galoy-agents"
  chart     = "${path.module}/chart"
  namespace = local.namespace

  values = [
    templatefile("${path.module}/prod-values.yml.tmpl", {
      image_digest              = var.image_digest
      sandbox_base_image_digest = var.sandbox_base_image_digest
    })
  ]

  dependency_update = true
  timeout           = 900 # 15 minutes

  depends_on = [
    kubernetes_secret.galoy_agents,
    kubernetes_namespace.sandbox,
    google_container_node_pool.gvisor,
    kubectl_manifest.sandbox_controller,
    kubectl_manifest.sandbox_extensions,
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

provider "helm" {
  kubernetes {
    host                   = "https://${data.google_container_cluster.primary.private_cluster_config.0.private_endpoint}"
    token                  = data.google_client_config.default.access_token
    cluster_ca_certificate = base64decode(data.google_container_cluster.primary.master_auth.0.cluster_ca_certificate)
  }
}
