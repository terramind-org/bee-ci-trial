variable "name" {
  description = "Prefix used to name blob storage resources."
  type        = string
}

variable "location" {
  description = "GCS bucket location."
  type        = string
}

variable "backup_retention_days" {
  description = "Days to retain objects below the backups/ prefix."
  type        = number

  validation {
    condition     = var.backup_retention_days >= 1
    error_message = "backup_retention_days must be at least one."
  }
}
