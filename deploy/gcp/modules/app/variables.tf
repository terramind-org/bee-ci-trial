variable "namespace" {
  description = "Kubernetes namespace for the Beehive release."
  type        = string
}

variable "release_name" {
  description = "Helm release name for Beehive."
  type        = string
  default     = "bee"
}

variable "image_tag" {
  description = "Beehive server and web image tag to deploy."
  type        = string
  default     = "0.1.0"
}

variable "server_replicas" {
  description = "Number of Beehive server replicas."
  type        = number
}

variable "site_admins" {
  description = "Principal ids of the site administrators; see the first-admin steps in docs/self-hosting.md."
  type        = list(string)
  default     = []
}

variable "domain" {
  description = "Public DNS domain for Beehive."
  type        = string
}

variable "tls_secret_name" {
  description = "TLS Secret name used by the Beehive Ingress."
  type        = string
}

variable "database_host" {
  description = "Private address of the Cloud SQL instance."
  type        = string
}

variable "database_name" {
  description = "Beehive PostgreSQL database name."
  type        = string
}

variable "database_user" {
  description = "Beehive PostgreSQL application user."
  type        = string
}

variable "database_password" {
  description = "Beehive PostgreSQL application password."
  type        = string
  sensitive   = true
}

variable "s3_access_key" {
  description = "S3-interoperable access key for blob storage."
  type        = string
}

variable "s3_secret_key" {
  description = "S3-interoperable secret key for blob storage."
  type        = string
  sensitive   = true
}

variable "storage_bucket" {
  description = "GCS bucket exposed through the S3 API."
  type        = string
}

variable "signing_key_hex" {
  description = "Shared hexadecimal token signing key."
  type        = string
  sensitive   = true
}

variable "url_key_hex" {
  description = "Shared hexadecimal signed-URL key."
  type        = string
  sensitive   = true
}

variable "oidc_key_pkcs8_hex" {
  description = "PKCS#8 DER OIDC signing key encoded as hexadecimal; convert the Secret Manager PEM value manually as documented by modules/secrets."
  type        = string
  sensitive   = true
}
