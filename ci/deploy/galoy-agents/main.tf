variable "image_digest" {}
variable "github_client_secret" {}
variable "concourse_username" {}
variable "concourse_password" {}

locals {
  cluster_name     = "galoy-agents-cluster"
  cluster_location = "us-east1-b"
  gcp_project      = "galoy-agents"
  namespace        = "galoy-agents"
  vpc_name         = "galoy-agents-vpc"
  region           = "us-east1"
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
  content = file("${path.module}/chart/vendor/agent-sandbox/manifest.yaml")
}

resource "kubectl_manifest" "sandbox_controller" {
  for_each  = data.kubectl_file_documents.sandbox_controller.manifests
  yaml_body = each.value

  depends_on = [google_container_node_pool.gvisor]
}

data "kubectl_file_documents" "sandbox_extensions" {
  content = file("${path.module}/chart/vendor/agent-sandbox/extensions.yaml")
}

resource "kubectl_manifest" "sandbox_extensions" {
  for_each  = data.kubectl_file_documents.sandbox_extensions.manifests
  yaml_body = each.value

  depends_on = [google_container_node_pool.gvisor]
}

resource "helm_release" "galoy_agents" {
  name      = "galoy-agents"
  chart     = "${path.module}/chart"
  namespace = local.namespace

  values = [
    templatefile("${path.module}/prod-values.yml.tmpl", {
      image_digest = var.image_digest
    })
  ]

  dependency_update = true
  timeout           = 900 # 15 minutes

  depends_on = [
    kubernetes_secret.galoy_agents,
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
