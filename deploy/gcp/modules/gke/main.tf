resource "google_container_cluster" "this" {
  name                = "${var.name}-gke"
  location            = var.region
  network             = var.network
  subnetwork          = var.subnetwork
  enable_autopilot    = true
  deletion_protection = true

  release_channel {
    channel = "REGULAR"
  }

  ip_allocation_policy {
    cluster_secondary_range_name  = var.pod_secondary_range_name
    services_secondary_range_name = var.service_secondary_range_name
  }

  private_cluster_config {
    enable_private_nodes    = true
    enable_private_endpoint = false
  }

  master_authorized_networks_config {
    dynamic "cidr_blocks" {
      for_each = var.master_authorized_networks

      content {
        cidr_block   = cidr_blocks.value.cidr_block
        display_name = cidr_blocks.value.display_name
      }
    }
  }

  gateway_api_config {
    channel = "CHANNEL_STANDARD"
  }

  # Autopilot clusters always enable Workload Identity.
}
