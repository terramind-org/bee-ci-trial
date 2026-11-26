resource "google_compute_address" "this" {
  name         = "${var.name}-ingress"
  region       = var.region
  address_type = "EXTERNAL"
  network_tier = "PREMIUM"
}

resource "helm_release" "ingress_nginx" {
  name             = "ingress-nginx"
  repository       = "https://kubernetes.github.io/ingress-nginx"
  chart            = "ingress-nginx"
  version          = "4.12.1"
  namespace        = "ingress-nginx"
  create_namespace = true

  set {
    name  = "controller.service.loadBalancerIP"
    value = google_compute_address.this.address
  }
}

# ingress-nginx serves the chart's regex paths and h2c backend. GKE Ingress cannot do both,
# and Google-managed certificates cannot attach to ingress-nginx. Operators provide the TLS
# Secret named by tls_secret_name; with cert-manager enabled, apply
# deploy/gcp/manifests/cluster-issuer.example.yaml after this release is installed.
resource "helm_release" "cert_manager" {
  count            = var.install_cert_manager ? 1 : 0
  name             = "cert-manager"
  repository       = "https://charts.jetstack.io"
  chart            = "cert-manager"
  version          = "v1.16.2"
  namespace        = "cert-manager"
  create_namespace = true

  set {
    name  = "crds.enabled"
    value = "true"
  }
}

resource "google_dns_record_set" "bee" {
  count        = var.dns_managed_zone == "" ? 0 : 1
  managed_zone = var.dns_managed_zone
  name         = "${var.domain}."
  type         = "A"
  ttl          = 300
  rrdatas      = [google_compute_address.this.address]
}
