output "network_id" {
  description = "Identifier of the VPC network."
  value       = google_compute_network.this.id
}

output "network_self_link" {
  description = "Self link of the VPC network."
  value       = google_compute_network.this.self_link
}

output "subnet_self_link" {
  description = "Self link of the regional subnet."
  value       = google_compute_subnetwork.this.self_link
}

output "pod_secondary_range_name" {
  description = "Name of the subnet secondary range used by GKE pods."
  value       = google_compute_subnetwork.this.secondary_ip_range[0].range_name
}

output "service_secondary_range_name" {
  description = "Name of the subnet secondary range used by GKE Services."
  value       = google_compute_subnetwork.this.secondary_ip_range[1].range_name
}

output "private_services_connection_id" {
  description = "Identifier of the private services access connection."
  value       = google_service_networking_connection.private_services.id
}
