variable "name" {
  description = "Prefix used to name network resources."
  type        = string
}

variable "region" {
  description = "GCP region containing the subnet and Cloud NAT."
  type        = string
}

variable "subnet_range" {
  description = "Primary IPv4 CIDR range for the regional subnet."
  type        = string
}

variable "pod_range" {
  description = "IPv4 CIDR range allocated to GKE pods."
  type        = string
}

variable "service_range" {
  description = "IPv4 CIDR range allocated to GKE Services."
  type        = string
}
