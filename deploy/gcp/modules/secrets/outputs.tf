output "token_signing_key" {
  description = "Hex token signing key stored in Secret Manager."
  value       = random_id.token_signing_key.hex
  sensitive   = true
}

output "blob_url_key" {
  description = "Hex blob URL signing key stored in Secret Manager."
  value       = random_id.blob_url_key.hex
  sensitive   = true
}

output "oidc_signing_key_pem_pkcs8" {
  description = "PKCS#8 PEM OIDC signing key stored in Secret Manager."
  value       = tls_private_key.oidc_signing_key.private_key_pem_pkcs8
  sensitive   = true
}

output "database_password" {
  description = "Database password stored in Secret Manager."
  value       = random_password.database.result
  sensitive   = true
}
