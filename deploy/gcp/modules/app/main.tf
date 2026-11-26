resource "kubernetes_namespace_v1" "this" {
  metadata {
    name = var.namespace
  }
}

# Chart key names from deploy/helm/bee/values.yaml lines 94-113:
# postgres-url, forge-database, s3-access-key, s3-secret-key, signing-key-hex,
# url-key-hex, and oidc-key-pkcs8-hex. The OIDC key is supplied as DER hex because
# modules/secrets retains its Secret Manager value as PEM for an operator conversion step.
resource "kubernetes_secret_v1" "bee" {
  metadata {
    name      = "${var.release_name}-secrets"
    namespace = kubernetes_namespace_v1.this.metadata[0].name
  }

  data = {
    "postgres-url" = format(
      "postgres://%s:%s@%s:5432/%s",
      var.database_user,
      var.database_password,
      var.database_host,
      var.database_name,
    )
    "forge-database" = format(
      "postgres://%s:%s@%s:5432/%s?schema=bee_forge",
      var.database_user,
      var.database_password,
      var.database_host,
      var.database_name,
    )
    "s3-access-key"      = var.s3_access_key
    "s3-secret-key"      = var.s3_secret_key
    "signing-key-hex"    = var.signing_key_hex
    "url-key-hex"        = var.url_key_hex
    "oidc-key-pkcs8-hex" = var.oidc_key_pkcs8_hex
  }

  type = "Opaque"
}

resource "helm_release" "bee" {
  name      = var.release_name
  chart     = "${path.module}/../../../helm/bee"
  namespace = kubernetes_namespace_v1.this.metadata[0].name

  set {
    name  = "server.image.repository"
    value = "ghcr.io/terramind-org/bee-server"
  }

  set {
    name  = "server.image.tag"
    value = var.image_tag
  }

  set {
    name  = "web.image.repository"
    value = "ghcr.io/terramind-org/bee-web"
  }

  set {
    name  = "web.image.tag"
    value = var.image_tag
  }

  set {
    name  = "server.replicas"
    value = var.server_replicas
  }

  set {
    name  = "config.edition"
    value = "cloud"
  }

  dynamic "set_list" {
    for_each = length(var.site_admins) > 0 ? [var.site_admins] : []

    content {
      name  = "config.siteAdmins"
      value = set_list.value
    }
  }

  set {
    name  = "config.s3.endpoint"
    value = "https://storage.googleapis.com"
  }

  set {
    name  = "config.s3.bucket"
    value = var.storage_bucket
  }

  set {
    name  = "config.publicUrl"
    value = "https://${var.domain}"
  }

  set {
    name  = "secrets.existingSecret"
    value = kubernetes_secret_v1.bee.metadata[0].name
  }

  set {
    name  = "ingress.enabled"
    value = "true"
  }

  set {
    name  = "ingress.className"
    value = "nginx"
  }

  set {
    name  = "ingress.host"
    value = var.domain
  }

  set {
    name  = "ingress.tlsSecretName"
    value = var.tls_secret_name
  }
}
