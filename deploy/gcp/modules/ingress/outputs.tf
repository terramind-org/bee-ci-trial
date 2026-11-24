output "ip_address" {
  description = "Regional external IP address assigned to ingress-nginx."
  value       = google_compute_address.this.address
}
