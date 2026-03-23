module "bootstrap" {
  source = "git::https://github.com/GaloyMoney/galoy-infra.git//modules/bootstrap/gcp?ref=4666137"

  name_prefix                   = var.name_prefix
  gcp_project                   = var.gcp_project
  enable_services               = var.enable_services
  tf_state_bucket_force_destroy = var.tf_state_bucket_force_destroy
  tf_state_bucket_location      = var.tf_state_bucket_location
}
