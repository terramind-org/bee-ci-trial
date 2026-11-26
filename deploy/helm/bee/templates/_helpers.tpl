{{/* Chart name, truncated to fit a DNS label. */}}
{{- define "bee.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{/* Release-scoped name: <release>-<chart>, or just the release when it already contains the chart name. */}}
{{- define "bee.fullname" -}}
{{- if .Values.fullnameOverride -}}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- $name := default .Chart.Name .Values.nameOverride -}}
{{- if contains $name .Release.Name -}}
{{- .Release.Name | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- printf "%s-%s" .Release.Name $name | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}
{{- end -}}

{{- define "bee.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{/* Labels every object carries. */}}
{{- define "bee.labels" -}}
helm.sh/chart: {{ include "bee.chart" . }}
app.kubernetes.io/name: {{ include "bee.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end -}}

{{/* Selector labels; pass a dict with "ctx" (root context) and "component". */}}
{{- define "bee.selectorLabels" -}}
app.kubernetes.io/name: {{ include "bee.name" .ctx }}
app.kubernetes.io/instance: {{ .ctx.Release.Name }}
app.kubernetes.io/component: {{ .component }}
{{- end -}}

{{- define "bee.serviceAccountName" -}}
{{- if .Values.serviceAccount.create -}}
{{- default (include "bee.fullname" .) .Values.serviceAccount.name -}}
{{- else -}}
{{- default "default" .Values.serviceAccount.name -}}
{{- end -}}
{{- end -}}

{{/* image: <repository>[:<tag>|@<digest>]; the tag defaults to the chart's appVersion. */}}
{{- define "bee.image" -}}
{{- $img := .image -}}
{{- if $img.digest -}}
{{- printf "%s@%s" $img.repository $img.digest -}}
{{- else -}}
{{- printf "%s:%s" $img.repository (default .ctx.Chart.AppVersion $img.tag) -}}
{{- end -}}
{{- end -}}

{{/* The ConfigMap that holds server.toml. */}}
{{- define "bee.configMapName" -}}
{{- default (printf "%s-server" (include "bee.fullname" .)) .Values.config.existingConfigMap -}}
{{- end -}}

{{/* One secretKeyRef environment variable. Pass dict "ctx", "name" (variable), "key" (key in the Secret). */}}
{{- define "bee.secretEnv" -}}
- name: {{ .name }}
  valueFrom:
    secretKeyRef:
      name: {{ required "secrets.existingSecret is required: the name of a Secret that holds the credentials (see README)" .ctx.Values.secrets.existingSecret }}
      key: {{ .key }}
{{- end -}}

{{/* The environment the server and its --check-config init container share. */}}
{{- define "bee.server.env" -}}
- name: POD_IP
  valueFrom:
    fieldRef:
      fieldPath: status.podIP
# Other replicas forward lands to the replica that owns a ref's coalescer at this address
# (server.advertise_url in server.toml).
- name: BEE_ADVERTISE_URL
  value: "http://$(POD_IP):7420"
- name: BEE_PUBLIC_URL
  value: {{ required "config.publicUrl is required: the URL agents, the CLI and git reach Beehive at, for example https://bee.example.com" .Values.config.publicUrl | quote }}
{{- if .Values.config.s3.endpoint }}
- name: BEE_S3_ENDPOINT
  value: {{ .Values.config.s3.endpoint | quote }}
{{- end }}
- name: BEE_S3_BUCKET
  value: {{ required "config.s3.bucket is required" .Values.config.s3.bucket | quote }}
{{ include "bee.secretEnv" (dict "ctx" . "name" "BEE_POSTGRES_URL" "key" .Values.secrets.keys.postgresUrl) }}
{{ include "bee.secretEnv" (dict "ctx" . "name" "BEE_FORGE_DATABASE" "key" .Values.secrets.keys.forgeDatabase) }}
{{ include "bee.secretEnv" (dict "ctx" . "name" "BEE_S3_ACCESS_KEY" "key" .Values.secrets.keys.s3AccessKey) }}
{{ include "bee.secretEnv" (dict "ctx" . "name" "BEE_S3_SECRET_KEY" "key" .Values.secrets.keys.s3SecretKey) }}
{{ include "bee.secretEnv" (dict "ctx" . "name" "BEE_SIGNING_KEY_HEX" "key" .Values.secrets.keys.signingKeyHex) }}
{{- if .Values.secrets.sharedKeys.enabled }}
{{ include "bee.secretEnv" (dict "ctx" . "name" "BEE_URL_KEY_HEX" "key" .Values.secrets.keys.urlKeyHex) }}
{{ include "bee.secretEnv" (dict "ctx" . "name" "BEE_OIDC_KEY_HEX" "key" .Values.secrets.keys.oidcKeyPkcs8Hex) }}
{{- end }}
{{- if .Values.ssh.enabled }}
- name: BEE_GIT_SSH_LISTEN
  value: "0.0.0.0:2222"
{{- if .Values.ssh.hostKeySecret.name }}
- name: BEE_GIT_SSH_HOST_KEY
  value: {{ printf "/etc/bee-ssh/%s" .Values.ssh.hostKeySecret.key | quote }}
{{- end }}
{{- end }}
{{- with .Values.server.extraEnv }}
{{ toYaml . }}
{{- end }}
{{- end -}}

{{/* topologySpreadConstraints with this component's labelSelector filled in. Pass dict "constraints" and "selector". */}}
{{- define "bee.topologySpread" -}}
{{- range .constraints }}
- {{- toYaml (merge (deepCopy .) (dict "labelSelector" (dict "matchLabels" $.selector))) | nindent 2 }}
{{- end }}
{{- end -}}

{{/* Refuses a configuration that runs several replicas without the keys they must share. */}}
{{- define "bee.validate" -}}
{{- $max := int .Values.server.replicas -}}
{{- if .Values.server.autoscaling.enabled -}}
{{- $max = int .Values.server.autoscaling.maxReplicas -}}
{{- end -}}
{{- if and (gt $max 1) (not .Values.secrets.sharedKeys.enabled) -}}
{{- fail (printf "secrets.sharedKeys.enabled is false but the server can run %d replicas. Replicas must share blobs.url_key_hex and auth.oidc_key_pkcs8_hex, or each pod signs URLs and OIDC tokens with its own key. Put url-key-hex and oidc-key-pkcs8-hex in the Secret %s (README, Install step 2) and leave secrets.sharedKeys.enabled true, or run a single replica." $max (default "named by secrets.existingSecret" .Values.secrets.existingSecret)) -}}
{{- end -}}
{{- end -}}
