terraform {
  backend "gcs" {
    bucket = "galoy-agents-tf-state"
    prefix = "galoy-agents/inception"
  }
}
