variable "gcp_project" {
  description = "GCP project ID"
  default     = "galoy-agents"
}

variable "name_prefix" {
  description = "Prefix for all resource names"
  default     = "galoy-agents"
}

variable "node_service_account" {
  description = "Service account email for GKE cluster nodes (from inception output)"
  type        = string
}

variable "cluster_zone" {
  description = "GKE cluster zone suffix (e.g. 'b' for us-east1-b)"
  default     = "b"
}

variable "node_default_machine_type" {
  description = "Machine type for default GKE node pool"
  default     = "e2-standard-2"
}

variable "min_default_node_count" {
  description = "Minimum number of nodes in the default pool"
  default     = 1
}

variable "max_default_node_count" {
  description = "Maximum number of nodes in the default pool"
  default     = 3
}
