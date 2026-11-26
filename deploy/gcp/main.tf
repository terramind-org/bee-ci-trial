module "network" {
  source = "./modules/network"

  name          = var.name
  region        = var.region
  subnet_range  = var.subnet_range
  pod_range     = var.pod_range
  service_range = var.service_range
}

module "gke" {
  source = "./modules/gke"

  name                         = var.name
  region                       = var.region
  network                      = module.network.network_id
  subnetwork                   = module.network.subnet_self_link
  pod_secondary_range_name     = module.network.pod_secondary_range_name
  service_secondary_range_name = module.network.service_secondary_range_name
  master_authorized_networks   = var.master_authorized_networks
}

module "secrets" {
  source = "./modules/secrets"

  name = var.name
}

module "database" {
  source = "./modules/database"

  name              = var.name
  region            = var.region
  network           = module.network.network_self_link
  database_name     = var.database_name
  database_user     = var.database_user
  database_password = module.secrets.database_password
  tier              = var.database_tier
  server_replicas   = var.server_replicas

  depends_on = [module.network]
}

module "storage" {
  source = "./modules/storage"

  name                  = var.name
  location              = var.region
  backup_retention_days = var.backup_retention_days
}

module "ingress" {
  source = "./modules/ingress"

  name                 = var.name
  region               = var.region
  domain               = var.domain
  tls_secret_name      = var.tls_secret_name
  install_cert_manager = var.install_cert_manager
  dns_managed_zone     = var.dns_managed_zone

  depends_on = [module.gke]
}

module "app" {
  source = "./modules/app"

  namespace          = var.namespace
  image_tag          = var.image_tag
  server_replicas    = var.server_replicas
  site_admins        = var.site_admins
  domain             = var.domain
  tls_secret_name    = var.tls_secret_name
  database_host      = module.database.private_ip
  database_name      = module.database.database_name
  database_user      = module.database.user
  database_password  = module.secrets.database_password
  s3_access_key      = module.storage.hmac_access_id
  s3_secret_key      = module.storage.hmac_secret
  storage_bucket     = module.storage.bucket_name
  signing_key_hex    = module.secrets.token_signing_key
  url_key_hex        = module.secrets.blob_url_key
  oidc_key_pkcs8_hex = var.oidc_key_pkcs8_hex

  depends_on = [module.ingress]
}
