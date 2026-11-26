output "network_self_link" {
  description = "Self link of the VPC network."
  value       = module.network.network_self_link
}

output "gke_cluster_name" {
  description = "Name of the GKE cluster."
  value       = module.gke.name
}

output "gke_cluster_endpoint" {
  description = "Public control-plane endpoint of the GKE cluster."
  value       = module.gke.endpoint
  sensitive   = true
}

output "gke_cluster_ca_certificate" {
  description = "Base64-encoded GKE cluster CA certificate."
  value       = module.gke.ca_certificate
  sensitive   = true
}

output "gke_cluster_location" {
  description = "Location of the regional GKE cluster."
  value       = module.gke.location
}

output "database_private_ip" {
  description = "Private IPv4 address of the Cloud SQL instance."
  value       = module.database.private_ip
}

output "database_name" {
  description = "Name of the Beehive PostgreSQL database."
  value       = module.database.database_name
}

output "database_user" {
  description = "Name of the Beehive PostgreSQL application user."
  value       = module.database.user
}

output "database_connection_name" {
  description = "Cloud SQL instance connection name."
  value       = module.database.connection_name
}

output "blob_bucket_name" {
  description = "Name of the GCS bucket used for Beehive blobs."
  value       = module.storage.bucket_name
}

output "blob_hmac_access_id" {
  description = "S3-interoperable HMAC access ID for Beehive blob storage."
  value       = module.storage.hmac_access_id
}

output "blob_hmac_secret" {
  description = "S3-interoperable HMAC secret for Beehive blob storage."
  value       = module.storage.hmac_secret
  sensitive   = true
}

output "ingress_ip_address" {
  description = "Regional external IP address serving the ingress-nginx controller."
  value       = module.ingress.ip_address
}

output "bee_namespace" {
  description = "Kubernetes namespace containing the Beehive Helm release."
  value       = module.app.namespace
}

output "bee_secret_name" {
  description = "Name of the Kubernetes Secret consumed by the Beehive chart."
  value       = module.app.secret_name
}
