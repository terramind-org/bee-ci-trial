output "private_ip" {
  description = "Private IPv4 address of the Cloud SQL instance."
  value       = google_sql_database_instance.this.private_ip_address
}

output "database_name" {
  description = "Name of the Beehive PostgreSQL database."
  value       = google_sql_database.this.name
}

output "user" {
  description = "Name of the Beehive PostgreSQL application user."
  value       = google_sql_user.this.name
}

output "connection_name" {
  description = "Cloud SQL instance connection name."
  value       = google_sql_database_instance.this.connection_name
}
