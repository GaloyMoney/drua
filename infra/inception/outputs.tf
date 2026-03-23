output "bastion_name" {
  description = "Name of the bastion host instance"
  value       = module.inception.bastion_name
}

output "bastion_zone" {
  description = "Zone of the bastion host instance"
  value       = module.inception.bastion_zone
}

output "cluster_sa" {
  description = "Service account for GKE cluster nodes"
  value       = module.inception.cluster_sa
}

output "backups_bucket_name" {
  description = "Name of the backups GCS bucket"
  value       = module.inception.backups_bucket_name
}

output "backups_sa" {
  description = "Service account for backups bucket access"
  value       = module.inception.backups_sa
}
