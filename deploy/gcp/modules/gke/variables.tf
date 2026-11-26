variable "name" {
  description = "Prefix used to name the GKE cluster."
  type        = string
}

variable "region" {
  description = "GCP region containing the regional GKE cluster."
  type        = string
}

variable "network" {
  description = "VPC network identifier for the GKE cluster."
  type        = string
}

variable "subnetwork" {
  description = "Subnetwork self link for the GKE cluster."
  type        = string
}

variable "pod_secondary_range_name" {
  description = "Name of the subnet secondary range used by GKE pods."
  type        = string
}

variable "service_secondary_range_name" {
  description = "Name of the subnet secondary range used by GKE Services."
  type        = string
}

variable "master_authorized_networks" {
  description = "CIDR blocks allowed to reach the public GKE control-plane endpoint."
  type = list(object({
    cidr_block   = string
    display_name = string
  }))
}
