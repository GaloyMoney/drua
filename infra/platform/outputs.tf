output "cluster_ca_cert" {
  description = "GKE cluster CA certificate"
  value       = module.platform.cluster_ca_cert
  sensitive   = true
}

output "master_endpoint" {
  description = "GKE cluster master endpoint URL"
  value       = module.platform.master_endpoint
}

output "cluster_name" {
  description = "GKE cluster name"
  value       = module.platform.cluster_name
}

output "cluster_location" {
  description = "GKE cluster location"
  value       = module.platform.cluster_location
}
