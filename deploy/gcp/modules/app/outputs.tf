output "namespace" {
  description = "Namespace containing the Beehive release."
  value       = kubernetes_namespace_v1.this.metadata[0].name
}

output "secret_name" {
  description = "Kubernetes Secret read by the Beehive chart."
  value       = kubernetes_secret_v1.bee.metadata[0].name
}
