output "inception_sa" {
  description = "Service account email for the inception phase"
  value       = module.bootstrap.inception_sa
}

output "tf_state_bucket_name" {
  description = "Name of the GCS bucket storing Terraform state"
  value       = module.bootstrap.tf_state_bucket_name
}

output "tf_state_bucket_location" {
  description = "Location of the Terraform state bucket"
  value       = module.bootstrap.tf_state_bucket_location
}
