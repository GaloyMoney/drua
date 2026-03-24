variable "galoy_agents_namespace" {
  description = "Namespace for the galoy-agents deployment"
  type        = string
  default     = "galoy-agents"
}

variable "galoy_agents_image_tag" {
  description = "Image tag for galoy-agents"
  type        = string
  default     = "edge"
}

variable "galoy_agents_secrets" {
  description = "JSON-encoded secrets for galoy-agents"
  sensitive   = true
  type        = string
  default     = "{}"
}

terraform {
  required_version = ">= 1.0"

  required_providers {
    kubernetes = {
      source  = "hashicorp/kubernetes"
      version = "~> 2.23"
    }
    helm = {
      source  = "hashicorp/helm"
      version = "~> 2.11"
    }
    random = {
      source  = "hashicorp/random"
      version = "~> 3.0"
    }
  }
}

provider "kubernetes" {
  config_path = "~/.kube/config"
}

provider "helm" {
  kubernetes {
    config_path = "~/.kube/config"
  }
}

locals {
  secrets = length(var.galoy_agents_secrets) > 2 ? jsondecode(var.galoy_agents_secrets) : {}

  pg_password = try(local.secrets.pg_password, "galoy-agents")
  pg_con      = try(local.secrets.pg_con, "postgresql://galoy-agents:${local.pg_password}@galoy-agents-postgresql:5432/galoy-agents")

  github_client_id     = try(local.secrets.github_client_id, "dummy-client-id")
  github_client_secret = try(local.secrets.github_client_secret, "dummy-client-secret")
  github_redirect_uri  = try(local.secrets.github_redirect_uri, "http://localhost:5254/auth/callback")
}

resource "kubernetes_namespace" "galoy_agents" {
  metadata {
    name = var.galoy_agents_namespace
  }
}

resource "kubernetes_secret" "galoy_agents" {
  metadata {
    name      = "galoy-agents"
    namespace = kubernetes_namespace.galoy_agents.metadata[0].name
  }

  data = {
    pg-user-pw           = local.pg_password
    pg-con               = local.pg_con
    github-client-id     = local.github_client_id
    github-client-secret = local.github_client_secret
    github-redirect-uri  = local.github_redirect_uri
  }
}

resource "helm_release" "galoy_agents" {
  name      = "galoy-agents"
  chart     = "${path.module}/../../charts/galoy-agents"
  namespace = kubernetes_namespace.galoy_agents.metadata[0].name

  values = [
    templatefile("${path.module}/galoy-agents-values.yml.tmpl", {
      image_tag = var.galoy_agents_image_tag
    })
  ]

  depends_on = [kubernetes_secret.galoy_agents]

  dependency_update = true
  timeout           = 900 # 15 minutes
}
