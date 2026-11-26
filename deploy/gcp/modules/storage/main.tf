resource "google_storage_bucket" "blobs" {
  name                        = "${var.name}-blobs"
  location                    = var.location
  uniform_bucket_level_access = true

  versioning {
    enabled = false
  }

  lifecycle_rule {
    action {
      type = "Delete"
    }

    condition {
      age            = var.backup_retention_days
      matches_prefix = ["backups/"]
    }
  }
}

resource "google_service_account" "blobs" {
  account_id   = "${var.name}-blobs"
  display_name = "Bee blob storage"
}

resource "google_storage_bucket_iam_member" "blobs_object_admin" {
  bucket = google_storage_bucket.blobs.name
  role   = "roles/storage.objectAdmin"
  member = "serviceAccount:${google_service_account.blobs.email}"
}

resource "google_storage_hmac_key" "blobs" {
  service_account_email = google_service_account.blobs.email
}

resource "google_secret_manager_secret" "blobs_hmac_secret" {
  secret_id = "${var.name}-blobs-hmac-secret"

  replication {
    auto {}
  }
}

resource "google_secret_manager_secret_version" "blobs_hmac_secret" {
  secret      = google_secret_manager_secret.blobs_hmac_secret.id
  secret_data = google_storage_hmac_key.blobs.secret
}

# Beehive uses the GCS S3-interoperable XML API with:
# [blobs]
# backend = "s3"
# s3_endpoint = "https://storage.googleapis.com"
