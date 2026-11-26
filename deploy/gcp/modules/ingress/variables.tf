variable "name" {
  description = "Prefix used to name ingress resources."
  type        = string
}

variable "region" {
  description = "GCP region for the regional external address."
  type        = string
}

variable "domain" {
  description = "DNS name served by the Beehive ingress."
  type        = string
}

variable "tls_secret_name" {
  description = "Name of the operator-supplied TLS Secret used by the Beehive Ingress."
  type        = string
}

variable "install_cert_manager" {
  description = "Whether to install cert-manager; apply manifests/cluster-issuer.example.yaml separately after installation."
  type        = bool
  default     = false
}

variable "dns_managed_zone" {
  description = "Existing Cloud DNS managed zone for domain, or an empty string to skip DNS management."
  type        = string
  default     = ""
}
