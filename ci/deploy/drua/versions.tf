terraform {
  # Bucket + prefix match ci/values.yml's deploy_tf_state_bucket and the
  # ci/pipeline.yml `tf-galoy-agents-prod` resource. `credentials` is
  # intentionally omitted — Concourse fills it in at apply time via the
  # terraform-resource override, and local users supply it via
  # `GOOGLE_APPLICATION_CREDENTIALS` env var or `gcloud auth
  # application-default login`. See ./Makefile for the local workflow.
  backend "gcs" {
    bucket = "galoy-agents-tf-state"
    prefix = "galoy-agents/prod"
  }

  required_providers {
    google = {
      source  = "hashicorp/google"
      version = "~> 6.0"
    }
    google-beta = {
      source  = "hashicorp/google-beta"
      version = "~> 6.0"
    }
    helm = {
      source  = "hashicorp/helm"
      version = "~> 2.0"
    }
    kubernetes = {
      source  = "hashicorp/kubernetes"
      version = "~> 2.0"
    }
    random = {
      source  = "hashicorp/random"
      version = "~> 3.0"
    }
    postgresql = {
      source  = "cyrilgdn/postgresql"
      version = "1.24.0"
    }
    kubectl = {
      source = "alekc/kubectl"
      # 2.3.0 has a broken combined provider schema under OpenTofu.
      version = "2.3.1"
    }
  }
}
