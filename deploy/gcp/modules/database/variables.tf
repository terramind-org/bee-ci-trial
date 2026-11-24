variable "name" {
  description = "Prefix used to name the Cloud SQL instance."
  type        = string
}

variable "region" {
  description = "GCP region containing the Cloud SQL instance."
  type        = string
}

variable "network" {
  description = "VPC network self link used for private IP."
  type        = string
}

variable "database_name" {
  description = "Name of the PostgreSQL database."
  type        = string
}

variable "database_user" {
  description = "Name of the PostgreSQL application user."
  type        = string
}

variable "database_password" {
  description = "Password for the PostgreSQL application user."
  type        = string
  sensitive   = true
}

variable "tier" {
  description = "Cloud SQL machine tier."
  type        = string
}

variable "server_replicas" {
  description = "Number of Beehive server replicas sharing this database."
  type        = number

  validation {
    condition     = var.server_replicas >= 1
    error_message = "server_replicas must be at least one."
  }
}
