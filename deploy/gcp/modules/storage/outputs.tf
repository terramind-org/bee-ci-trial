output "bucket_name" {
  description = "Name of the GCS bucket used for Beehive blobs."
  value       = google_storage_bucket.blobs.name
}

output "hmac_access_id" {
  description = "S3-interoperable HMAC access ID for the blob storage service account."
  value       = google_storage_hmac_key.blobs.access_id
}

output "hmac_secret" {
  description = "S3-interoperable HMAC secret stored in Secret Manager."
  value       = google_storage_hmac_key.blobs.secret
  sensitive   = true
}
