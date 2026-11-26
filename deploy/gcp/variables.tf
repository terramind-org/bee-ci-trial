variable "project_id" {
  description = "GCP project ID that will contain Beehive infrastructure."
  type        = string

  validation {
    condition     = length(trimspace(var.project_id)) > 0
    error_message = "project_id must not be empty."
  }
}

variable "region" {
  description = "GCP region used for regional infrastructure."
  type        = string
  default     = "us-central1"

  validation {
    condition     = can(regex("^[a-z]+-[a-z]+[0-9]+$", var.region))
    error_message = "region must use a GCP regional name such as us-central1."
  }
}

variable "name" {
  description = "Lowercase prefix used to name Beehive infrastructure."
  type        = string
  default     = "bee"

  validation {
    condition     = can(regex("^[a-z][a-z0-9-]*[a-z0-9]$", var.name))
    error_message = "name must contain lowercase letters, digits, and hyphens, and begin with a letter."
  }
}

variable "domain" {
  description = "Public DNS domain that will serve Beehive."
  type        = string

  validation {
    condition     = length(trimspace(var.domain)) > 0
    error_message = "domain must not be empty."
  }
}

variable "server_replicas" {
  description = "Number of Beehive server replicas to run."
  type        = number
  default     = 2

  validation {
    condition     = var.server_replicas >= 1
    error_message = "server_replicas must be at least one."
  }
}

variable "site_admins" {
  description = "Principal ids of the site administrators; see the first-admin steps in docs/self-hosting.md."
  type        = list(string)
  default     = []
}

variable "subnet_range" {
  description = "Primary IPv4 CIDR range for the regional subnet."
  type        = string
  default     = "10.10.0.0/20"

  validation {
    condition     = can(cidrhost(var.subnet_range, 0))
    error_message = "subnet_range must be a valid IPv4 CIDR range."
  }
}

variable "pod_range" {
  description = "IPv4 CIDR range allocated to GKE pods."
  type        = string
  default     = "10.20.0.0/16"

  validation {
    condition     = can(cidrhost(var.pod_range, 0))
    error_message = "pod_range must be a valid IPv4 CIDR range."
  }
}

variable "service_range" {
  description = "IPv4 CIDR range allocated to GKE Services."
  type        = string
  default     = "10.30.0.0/20"

  validation {
    condition     = can(cidrhost(var.service_range, 0))
    error_message = "service_range must be a valid IPv4 CIDR range."
  }
}

variable "master_authorized_networks" {
  description = "CIDR blocks allowed to reach the public GKE control-plane endpoint."
  type = list(object({
    cidr_block   = string
    display_name = string
  }))

  validation {
    condition = alltrue([
      for network in var.master_authorized_networks : can(cidrhost(network.cidr_block, 0))
    ])
    error_message = "Every master authorized network must have a valid IPv4 CIDR range."
  }
}

variable "database_name" {
  description = "Name of the PostgreSQL database used by Beehive."
  type        = string
  default     = "bee"

  validation {
    condition     = can(regex("^[a-z][a-z0-9_]*$", var.database_name))
    error_message = "database_name must start with a lowercase letter and contain only lowercase letters, digits, and underscores."
  }
}

variable "database_user" {
  description = "Name of the PostgreSQL application user used by Beehive."
  type        = string
  default     = "bee"

  validation {
    condition     = can(regex("^[a-z][a-z0-9_]*$", var.database_user))
    error_message = "database_user must start with a lowercase letter and contain only lowercase letters, digits, and underscores."
  }
}

variable "database_tier" {
  description = "Cloud SQL machine tier for PostgreSQL."
  type        = string
  default     = "db-custom-2-7680"

  validation {
    condition     = length(trimspace(var.database_tier)) > 0
    error_message = "database_tier must not be empty."
  }
}

variable "backup_retention_days" {
  description = "Days to retain GCS backup objects under the backups/ prefix."
  type        = number
  default     = 30

  validation {
    condition     = var.backup_retention_days >= 1
    error_message = "backup_retention_days must be at least one."
  }
}

variable "namespace" {
  description = "Kubernetes namespace for the Beehive Helm release."
  type        = string
  default     = "bee"
}

variable "image_tag" {
  description = "Beehive server and web image tag deployed by Helm."
  type        = string
  default     = "0.1.0"
}

variable "tls_secret_name" {
  description = "Operator-supplied Kubernetes TLS Secret used by the Beehive Ingress."
  type        = string
  default     = "bee-tls"
}

variable "install_cert_manager" {
  description = "Whether to install cert-manager before an operator applies the ClusterIssuer example."
  type        = bool
  default     = false
}

variable "dns_managed_zone" {
  description = "Existing Cloud DNS managed zone for domain, or an empty string to skip the A record."
  type        = string
  default     = ""
}

variable "oidc_key_pkcs8_hex" {
  description = "PKCS#8 DER OIDC signing key encoded as hex, converted from the Secret Manager PEM value documented in modules/secrets."
  type        = string
  sensitive   = true
}
