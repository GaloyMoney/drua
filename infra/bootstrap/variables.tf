variable "gcp_project" {
  description = "GCP project ID"
  default     = "galoy-agents"
}

variable "name_prefix" {
  description = "Prefix for all resource names"
  default     = "galoy-agents"
}

variable "tf_state_bucket_location" {
  description = "Location for the Terraform state bucket"
  default     = "US-EAST1"
}

variable "tf_state_bucket_force_destroy" {
  description = "Allow force-destroying the state bucket"
  default     = false
}

variable "enable_services" {
  description = "Enable required GCP APIs"
  default     = true
}
