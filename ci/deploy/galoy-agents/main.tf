variable "image_digest" {}

locals {
  cluster_name     = "galoy-agents-cluster"
  cluster_location = "us-east1-b"
  gcp_project      = "galoy-agents"
  namespace        = "galoy-agents"
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

provider "helm" {
  kubernetes {
    host                   = "https://${data.google_container_cluster.primary.private_cluster_config.0.private_endpoint}"
    token                  = data.google_client_config.default.access_token
    cluster_ca_certificate = base64decode(data.google_container_cluster.primary.master_auth.0.cluster_ca_certificate)
  }
}
