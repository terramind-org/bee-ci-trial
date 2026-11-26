resource "random_id" "token_signing_key" {
  byte_length = 32
}

resource "random_id" "blob_url_key" {
  byte_length = 32
}

resource "tls_private_key" "oidc_signing_key" {
  algorithm   = "ECDSA"
  ecdsa_curve = "P256"
}

resource "random_password" "database" {
  length  = 32
  special = false
}

resource "google_secret_manager_secret" "token_signing_key" {
  secret_id = "${var.name}-token-signing-key"

  replication {
    auto {}
  }
}

resource "google_secret_manager_secret_version" "token_signing_key" {
  secret      = google_secret_manager_secret.token_signing_key.id
  secret_data = random_id.token_signing_key.hex
}

resource "google_secret_manager_secret" "blob_url_key" {
  secret_id = "${var.name}-blob-url-key"

  replication {
    auto {}
  }
}

resource "google_secret_manager_secret_version" "blob_url_key" {
  secret      = google_secret_manager_secret.blob_url_key.id
  secret_data = random_id.blob_url_key.hex
}

resource "google_secret_manager_secret" "oidc_signing_key" {
  secret_id = "${var.name}-oidc-signing-key"

  replication {
    auto {}
  }
}

resource "google_secret_manager_secret_version" "oidc_signing_key" {
  secret      = google_secret_manager_secret.oidc_signing_key.id
  secret_data = tls_private_key.oidc_signing_key.private_key_pem_pkcs8
}

resource "google_secret_manager_secret" "database_password" {
  secret_id = "${var.name}-database-password"

  replication {
    auto {}
  }
}

resource "google_secret_manager_secret_version" "database_password" {
  secret      = google_secret_manager_secret.database_password.id
  secret_data = random_password.database.result
}

# auth.oidc_key_pkcs8_hex requires PKCS#8 DER represented as hex. Terraform cannot
# hex-encode binary DER, so convert the retrieved PEM manually:
# openssl pkcs8 -topk8 -nocrypt -outform DER -in key.pem | xxd -p | tr -d '\n'
