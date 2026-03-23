module "platform" {
  source = "git::https://github.com/GaloyMoney/galoy-infra.git//modules/platform/gcp?ref=4666137"

  name_prefix               = var.name_prefix
  gcp_project               = var.gcp_project
  node_service_account      = var.node_service_account
  node_default_machine_type = var.node_default_machine_type
  min_default_node_count    = var.min_default_node_count
  max_default_node_count    = var.max_default_node_count
  cluster_zone              = var.cluster_zone
  node_pool_zone            = "${var.region}-${var.cluster_zone}"
}
