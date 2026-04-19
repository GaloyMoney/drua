variable "drua_namespace" {
  description = "Namespace for the drua deployment"
  type        = string
  default     = "drua"
}

variable "drua_image_tag" {
  description = "Image tag for drua"
  type        = string
  default     = "edge"
}

variable "drua_secrets" {
  description = "JSON-encoded secrets for drua"
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
  secrets = length(var.drua_secrets) > 2 ? jsondecode(var.drua_secrets) : {}

  pg_password = try(local.secrets.pg_password, "drua")
  pg_con      = try(local.secrets.pg_con, "postgresql://drua:${local.pg_password}@drua-postgresql:5432/drua")

  github_client_id     = try(local.secrets.github_client_id, "dummy-client-id")
  github_client_secret = try(local.secrets.github_client_secret, "dummy-client-secret")
  github_redirect_uri  = try(local.secrets.github_redirect_uri, "http://localhost:5254/auth/callback")
}

resource "kubernetes_namespace" "drua" {
  metadata {
    name = var.drua_namespace
  }
}

resource "kubernetes_secret" "drua" {
  metadata {
    name      = "drua"
    namespace = kubernetes_namespace.drua.metadata[0].name
  }

  data = {
    pg-user-pw           = local.pg_password
    pg-con               = local.pg_con
    github-client-id     = local.github_client_id
    github-client-secret = local.github_client_secret
    github-redirect-uri  = local.github_redirect_uri
  }
}

resource "helm_release" "drua" {
  name      = "drua"
  chart     = "${path.module}/../../charts/drua"
  namespace = kubernetes_namespace.drua.metadata[0].name

  values = [
    templatefile("${path.module}/drua-values.yml.tmpl", {
      image_tag = var.drua_image_tag
    })
  ]

  depends_on = [kubernetes_secret.drua]

  dependency_update = true
  timeout           = 900 # 15 minutes
}
