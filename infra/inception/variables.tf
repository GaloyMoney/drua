variable "gcp_project" {
  description = "GCP project ID"
  default     = "galoy-agents"
}

variable "name_prefix" {
  description = "Prefix for all resource names"
  default     = "galoy-agents"
}

variable "inception_sa" {
  description = "Service account email from bootstrap phase"
  type        = string
  default     = "galoy-agents-inception-tf@galoy-agents.iam.gserviceaccount.com"
}

variable "tf_state_bucket_name" {
  description = "Name of the GCS bucket storing Terraform state"
  default     = "galoy-agents-tf-state"
}

variable "buckets_location" {
  description = "Location for GCS buckets"
  default     = "US-EAST1"
}

variable "backups_bucket_location" {
  description = "Location for the backups bucket"
  default     = "US-EAST1"
}

variable "cluster_zone" {
  description = "GKE cluster zone suffix (e.g. 'b' for us-east1-b)"
  default     = "b"
}

variable "users" {
  description = "List of users with role-based access flags"
  type = list(object({
    id        = string
    bastion   = bool
    inception = bool
    platform  = bool
    logs      = bool
  }))
  default = [
    {
      id        = "user:justin@galoy.io"
      bastion   = true
      inception = true
      platform  = true
      logs      = true
    },
  ]
}
