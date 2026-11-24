output "name" {
  description = "Name of the GKE cluster."
  value       = google_container_cluster.this.name
}

output "endpoint" {
  description = "Public control-plane endpoint of the GKE cluster."
  value       = google_container_cluster.this.endpoint
  sensitive   = true
}

output "ca_certificate" {
  description = "Base64-encoded cluster CA certificate."
  value       = google_container_cluster.this.master_auth[0].cluster_ca_certificate
  sensitive   = true
}

output "location" {
  description = "Location of the regional GKE cluster."
  value       = google_container_cluster.this.location
}
