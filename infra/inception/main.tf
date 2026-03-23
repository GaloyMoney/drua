module "inception" {
  source = "git::https://github.com/GaloyMoney/galoy-infra.git//modules/inception/gcp?ref=29a90ad"

  name_prefix             = var.name_prefix
  gcp_project             = var.gcp_project
  inception_sa            = var.inception_sa
  tf_state_bucket_name    = var.tf_state_bucket_name
  buckets_location        = var.buckets_location
  backups_bucket_location = var.backups_bucket_location
  cluster_zone            = var.cluster_zone
  users                   = var.users
}
