provider "google" {
  project = var.project_id
  region  = var.region
}

provider "google-beta" {
  project = var.project_id
  region  = var.region
}

data "google_client_config" "current" {}

provider "kubernetes" {
  host                   = "https://${module.gke.endpoint}"
  token                  = data.google_client_config.current.access_token
  cluster_ca_certificate = base64decode(module.gke.ca_certificate)
}

provider "helm" {
  kubernetes {
    host                   = "https://${module.gke.endpoint}"
    token                  = data.google_client_config.current.access_token
    cluster_ca_certificate = base64decode(module.gke.ca_certificate)
  }
}

# Create a versioned, access-restricted GCS bucket before enabling remote state.
# Then uncomment this backend block and set its bucket name for this environment.
# terraform {
#   backend "gcs" {
#     bucket = "bee-terraform-state"
#     prefix = "gcp"
#   }
# }
