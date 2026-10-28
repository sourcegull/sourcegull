{{/* Chart name. */}}
{{- define "sourcegull.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{/* Fully qualified app name, at most 63 chars (DNS label). */}}
{{- define "sourcegull.fullname" -}}
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

{{/* Name of a component resource: <fullname>-<component>. */}}
{{- define "sourcegull.componentName" -}}
{{- printf "%s-%s" (include "sourcegull.fullname" .root) .component | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "sourcegull.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{/*
Renders a value that may contain templates: a string goes through `tpl` as is, anything else as
YAML. Usage: include "sourcegull.tplvalues.render" (dict "value" .Values.x "context" $)
*/}}
{{- define "sourcegull.tplvalues.render" -}}
{{- $value := typeIs "string" .value | ternary .value (.value | toYaml) -}}
{{- if contains "{{" (toJson .value) -}}
{{- tpl $value .context -}}
{{- else -}}
{{- $value -}}
{{- end -}}
{{- end -}}

{{/* Common labels (with commonLabels; the chart's own win). */}}
{{- define "sourcegull.labels" -}}
{{- $labels := dict
  "helm.sh/chart" (include "sourcegull.chart" .)
  "app.kubernetes.io/name" (include "sourcegull.name" .)
  "app.kubernetes.io/instance" .Release.Name
  "app.kubernetes.io/version" .Chart.AppVersion
  "app.kubernetes.io/managed-by" .Release.Service
  "app.kubernetes.io/part-of" "sourcegull" -}}
{{- $common := fromYaml (include "sourcegull.tplvalues.render" (dict "value" (default dict .Values.commonLabels) "context" .)) -}}
{{- toYaml (merge $labels $common) -}}
{{- end -}}

{{/* Selector labels for a component. Usage: include "sourcegull.selectorLabels" (dict "root" $ "component" "web") */}}
{{- define "sourcegull.selectorLabels" -}}
app.kubernetes.io/name: {{ include "sourcegull.name" .root }}
app.kubernetes.io/instance: {{ .root.Release.Name }}
app.kubernetes.io/component: {{ .component }}
{{- end -}}

{{/* All labels for a component. */}}
{{- define "sourcegull.componentLabels" -}}
{{- toYaml (merge (dict "app.kubernetes.io/component" .component) (fromYaml (include "sourcegull.labels" .root))) -}}
{{- end -}}

{{/*
The `annotations:` block of a resource: `extra` (a dict) over commonAnnotations; nothing when
both are empty. Usage: include "sourcegull.annotations" (dict "root" $ "extra" $annotations)
*/}}
{{- define "sourcegull.annotations" -}}
{{- $annotations := merge (deepCopy (default dict .extra)) (default dict .root.Values.commonAnnotations) -}}
{{- with $annotations }}
annotations:
  {{- include "sourcegull.tplvalues.render" (dict "value" . "context" $.root) | nindent 2 }}
{{- end -}}
{{- end -}}

{{/*
Pod template labels: the selector labels over podLabels and commonLabels, plus the label that
lets a pod reach the bundled database and the repo nodes (`dbClient`).
Usage: include "sourcegull.podLabels" (dict "root" $ "component" "web" "values" .Values.web "dbClient" true)
*/}}
{{- define "sourcegull.podLabels" -}}
{{- $own := fromYaml (include "sourcegull.selectorLabels" .) -}}
{{- if .dbClient -}}
{{- $_ := set $own "sourcegull.io/db-client" "true" -}}
{{- end -}}
{{- $pod := fromYaml (include "sourcegull.tplvalues.render" (dict "value" (default dict .values.podLabels) "context" .root)) -}}
{{- $common := fromYaml (include "sourcegull.tplvalues.render" (dict "value" (default dict .root.Values.commonLabels) "context" .root)) -}}
{{- toYaml (merge $own $pod $common) -}}
{{- end -}}

{{/* Label marking pods that may connect to the bundled database and the repo nodes. */}}
{{- define "sourcegull.dbClientLabel" -}}
sourcegull.io/db-client: "true"
{{- end -}}

{{- define "sourcegull.serviceAccountName" -}}
{{- if .Values.serviceAccount.create -}}
{{- default (include "sourcegull.fullname" .) .Values.serviceAccount.name -}}
{{- else -}}
{{- default "default" .Values.serviceAccount.name -}}
{{- end -}}
{{- end -}}

{{/*
Image reference: global.imageRegistry or the image's registry, repository, then digest or tag.
Usage: include "sourcegull.image" (dict "root" $ "image" .Values.image "defaultTag" "x")
*/}}
{{- define "sourcegull.image" -}}
{{- $registry := default .image.registry .root.Values.global.imageRegistry -}}
{{- $repo := .image.repository -}}
{{- if $registry -}}
{{- $repo = printf "%s/%s" (trimSuffix "/" $registry) $repo -}}
{{- end -}}
{{- if .image.digest -}}
{{- printf "%s@%s" $repo .image.digest -}}
{{- else -}}
{{- printf "%s:%s" $repo (toString (default .defaultTag .image.tag)) -}}
{{- end -}}
{{- end -}}

{{- define "sourcegull.sourcegullImage" -}}
{{- include "sourcegull.image" (dict "root" . "image" .Values.image "defaultTag" .Chart.AppVersion) -}}
{{- end -}}

{{/* Names of the pull secrets: global.imagePullSecrets plus each image's pullSecrets (names or { name }). */}}
{{- define "sourcegull.pullSecretNames" -}}
{{- $names := list -}}
{{- $secrets := .root.Values.global.imagePullSecrets -}}
{{- range .images }}{{ $secrets = concat $secrets (default list .pullSecrets) }}{{ end -}}
{{- range $secrets -}}
{{- if kindIs "string" . }}{{ $names = append $names . }}{{ else }}{{ $names = append $names .name }}{{ end -}}
{{- end -}}
{{- join "," (uniq $names) -}}
{{- end -}}

{{/* The `imagePullSecrets:` block of a pod. Usage: include "sourcegull.imagePullSecrets" (dict "root" $ "images" (list .Values.image)) */}}
{{- define "sourcegull.imagePullSecrets" -}}
{{- with (include "sourcegull.pullSecretNames" .) }}
imagePullSecrets:
{{- range splitList "," . }}
  - name: {{ . }}
{{- end }}
{{- end -}}
{{- end -}}

{{/* Validates value combinations; included once from a template that always renders. */}}
{{- define "sourcegull.validate" -}}
{{- $pgMode := .Values.postgresql.mode -}}
{{- if not (has $pgMode (list "bundled" "external" "cnpg")) -}}
{{- fail "postgresql.mode must be bundled, external or cnpg" -}}
{{- end -}}
{{- if and (eq $pgMode "external") (not (include "sourcegull.externalDatabase.secretName" .)) -}}
{{- fail "postgresql.mode=external needs the database URL: set externalDatabase.existingSecret or existingSecret" -}}
{{- end -}}
{{- $w := .Values.worker.autoscaling -}}
{{- if and $w.hpa.enabled $w.keda.enabled -}}
{{- fail "worker.autoscaling.hpa.enabled and worker.autoscaling.keda.enabled are exclusive (KEDA creates its own HPA)" -}}
{{- end -}}
{{- if $w.keda.enabled -}}
{{- if not (has $w.keda.trigger (list "postgresql" "prometheus")) -}}
{{- fail "worker.autoscaling.keda.trigger must be postgresql or prometheus" -}}
{{- end -}}
{{- if and (eq $w.keda.trigger "prometheus") (not $w.keda.prometheus.serverAddress) -}}
{{- fail "worker.autoscaling.keda.prometheus.serverAddress is required when worker.autoscaling.keda.trigger=prometheus" -}}
{{- end -}}
{{- end -}}
{{- if eq .Values.blobstore.mode "s3" -}}
{{- if not .Values.blobstore.s3.bucket -}}
{{- fail "blobstore.s3.bucket is required when blobstore.mode=s3" -}}
{{- end -}}
{{- if not .Values.blobstore.s3.existingSecret -}}
{{- fail "blobstore.s3.existingSecret is required when blobstore.mode=s3 (keys blobstore.s3.existingSecretAccessKeyIdKey and blobstore.s3.existingSecretSecretAccessKeyKey)" -}}
{{- end -}}
{{- else if ne .Values.blobstore.mode "builtin" -}}
{{- fail "blobstore.mode must be builtin or s3" -}}
{{- end -}}
{{- end -}}

{{/* Secret holding `encryption-keys` (and `database-url` for an external database). */}}
{{- define "sourcegull.appSecretName" -}}
{{- default (include "sourcegull.componentName" (dict "root" . "component" "secrets")) .Values.existingSecret -}}
{{- end -}}

{{/* Secret holding the external database's URL; empty when none is given. */}}
{{- define "sourcegull.externalDatabase.secretName" -}}
{{- default .Values.existingSecret .Values.externalDatabase.existingSecret -}}
{{- end -}}

{{/* Whether the chart runs the database (bundled or CloudNativePG): "true" or "". */}}
{{- define "sourcegull.postgresql.inRelease" -}}
{{- if ne .Values.postgresql.mode "external" -}}true{{- end -}}
{{- end -}}

{{/* Host of the database the chart runs: the bundled Service, or the CNPG read-write Service. */}}
{{- define "sourcegull.postgresql.host" -}}
{{- if eq .Values.postgresql.mode "cnpg" -}}
{{- printf "%s-rw" (include "sourcegull.cnpg.clusterName" .) -}}
{{- else -}}
{{- include "sourcegull.postgresql.fullname" . -}}
{{- end -}}
{{- end -}}

{{/* CloudNativePG: the Cluster's name and the Secret the operator generates for the app owner. */}}
{{- define "sourcegull.cnpg.clusterName" -}}
{{- include "sourcegull.componentName" (dict "root" . "component" "pg") -}}
{{- end -}}

{{- define "sourcegull.cnpg.appSecretName" -}}
{{- printf "%s-app" (include "sourcegull.cnpg.clusterName" .) -}}
{{- end -}}

{{/* Bundled PostgreSQL: service host and password Secret. */}}
{{- define "sourcegull.postgresql.fullname" -}}
{{- include "sourcegull.componentName" (dict "root" . "component" "postgresql") -}}
{{- end -}}

{{- define "sourcegull.postgresql.secretName" -}}
{{- default (include "sourcegull.postgresql.fullname" .) .Values.postgresql.auth.existingSecret -}}
{{- end -}}

{{- define "sourcegull.postgresql.secretKey" -}}
{{- if .Values.postgresql.auth.existingSecret -}}
{{- .Values.postgresql.auth.existingSecretPasswordKey -}}
{{- else -}}
password
{{- end -}}
{{- end -}}

{{/*
Reuses a key of an existing Secret (so generated values survive upgrades), or generates a new
value. Usage: include "sourcegull.secretValue" (dict "root" $ "name" "<secret>" "key" "<key>" "generate" "<value>")
Returns the base64-encoded value.
*/}}
{{- define "sourcegull.secretValue" -}}
{{- $existing := lookup "v1" "Secret" .root.Release.Namespace .name -}}
{{- if and $existing $existing.data (hasKey $existing.data .key) -}}
{{- index $existing.data .key -}}
{{- else -}}
{{- .generate | b64enc -}}
{{- end -}}
{{- end -}}

{{/* Database URL, logging and shutdown environment of every Sourcegull container (web, worker, repo, blob, migrate). */}}
{{- define "sourcegull.env" -}}
{{- $pgMode := .Values.postgresql.mode }}
{{- if eq $pgMode "cnpg" }}
# The `uri` key of the Secret CloudNativePG generates for the database owner.
- name: SOURCEGULL_DATABASE_URL
  valueFrom:
    secretKeyRef:
      name: {{ include "sourcegull.cnpg.appSecretName" . }}
      key: uri
{{- else if eq $pgMode "bundled" }}
- name: SOURCEGULL_DB_PASSWORD
  valueFrom:
    secretKeyRef:
      name: {{ include "sourcegull.postgresql.secretName" . }}
      key: {{ include "sourcegull.postgresql.secretKey" . }}
- name: SOURCEGULL_DATABASE_URL
  value: {{ printf "postgres://%s:$(SOURCEGULL_DB_PASSWORD)@%s:5432/%s?sslmode=disable" .Values.postgresql.auth.username (include "sourcegull.postgresql.fullname" .) .Values.postgresql.auth.database | quote }}
{{- else }}
- name: SOURCEGULL_DATABASE_URL
  valueFrom:
    secretKeyRef:
      name: {{ include "sourcegull.externalDatabase.secretName" . }}
      key: {{ .Values.externalDatabase.existingSecretUrlKey }}
{{- end }}
- name: SOURCEGULL_LOG_FORMAT
  value: {{ .Values.logFormat | quote }}
- name: RUST_LOG
  value: {{ .Values.logLevel | quote }}
# The pods' `preStop` hook pauses before SIGTERM, so the process stops without a further delay.
- name: SOURCEGULL_SHUTDOWN_DELAY
  value: "0"
{{- end -}}

{{/*
Repo node addresses: the StatefulSet pods' stable DNS names behind the headless Service,
comma-separated. Namespace-relative, so the persisted assignments (`repo.shard`) survive a move to
another namespace.
*/}}
{{- define "sourcegull.repoNodes" -}}
{{- if .Values.repo.enabled -}}
{{- $name := include "sourcegull.componentName" (dict "root" . "component" "repo") -}}
{{- $nodes := list -}}
{{- range $i := until (int .Values.repo.replicaCount) -}}
{{- $nodes = append $nodes (printf "http://%s-%d.%s:%d" $name $i $name (int $.Values.repo.service.ports.grpc)) -}}
{{- end -}}
{{- join "," $nodes -}}
{{- end -}}
{{- end -}}

{{/* The blob role serves its PVC-backed local store on the internal listener. */}}
{{- define "sourcegull.blobStoreURL" -}}
{{- if eq .Values.blobstore.mode "s3" -}}
{{- $s3 := .Values.blobstore.s3 -}}
{{- printf "s3://%s/%s" $s3.bucket (trimAll "/" $s3.prefix) | trimSuffix "/" -}}
{{- else -}}
{{- printf "http://%s:%d" (include "sourcegull.componentName" (dict "root" . "component" "blob")) (int .Values.blobstore.service.ports.internal) -}}
{{- end -}}
{{- end -}}

{{/* Blob store environment for web and worker: the blob role, or an S3 bucket. */}}
{{- define "sourcegull.blobStoreEnv" -}}
- name: SOURCEGULL_BLOB_STORE
  value: {{ include "sourcegull.blobStoreURL" . | quote }}
{{- if eq .Values.blobstore.mode "s3" }}
{{- $s3 := .Values.blobstore.s3 }}
- name: SOURCEGULL_S3_ENDPOINT
  value: {{ $s3.endpoint | quote }}
- name: SOURCEGULL_S3_REGION
  value: {{ $s3.region | quote }}
- name: SOURCEGULL_S3_PATH_STYLE
  value: {{ $s3.forcePathStyle | quote }}
- name: SOURCEGULL_S3_CREATE_BUCKET
  value: {{ $s3.createBucket | quote }}
- name: SOURCEGULL_S3_ACCESS_KEY_ID
  valueFrom:
    secretKeyRef:
      name: {{ $s3.existingSecret }}
      key: {{ $s3.existingSecretAccessKeyIdKey }}
- name: SOURCEGULL_S3_SECRET_ACCESS_KEY
  valueFrom:
    secretKeyRef:
      name: {{ $s3.existingSecret }}
      key: {{ $s3.existingSecretSecretAccessKeyKey }}
{{- if $s3.caConfigMap }}
- name: SOURCEGULL_S3_CA_CERT
  value: /etc/sourcegull/s3-ca/{{ $s3.caConfigMapKey }}
{{- end }}
{{- end }}
{{- end -}}

{{/* Environment for long-running roles (web, worker, repo, blob). */}}
{{- define "sourcegull.runtimeEnv" -}}
{{ include "sourcegull.env" . | trim }}
- name: SOURCEGULL_EXTERNAL_URL
  value: {{ .Values.externalURL | quote }}
- name: SOURCEGULL_REPO_NODES
  value: {{ include "sourcegull.repoNodes" . | quote }}
{{- with (include "sourcegull.keysEnv" . | trim) }}
{{ . }}
{{- end }}
{{- with (include "sourcegull.configEnv" . | trim) }}
{{ . }}
{{- end }}
- name: SOURCEGULL_DATA_DIR
  value: /var/opt/sourcegull
- name: SOURCEGULL_RATE_LIMIT_BACKEND
  value: {{ .Values.rateLimit.backend | quote }}
{{- end -}}

{{/* The encryption keys' variable (encryptionKeys.source): the key file or the keys. */}}
{{- define "sourcegull.keysEnv" -}}
{{- if eq .Values.encryptionKeys.source "file" }}
- name: SOURCEGULL_KEYS_FILE
  value: /etc/sourcegull/keys/encryption-keys
{{- else if eq .Values.encryptionKeys.source "env" }}
- name: SOURCEGULL_KEYS
  valueFrom:
    secretKeyRef:
      name: {{ include "sourcegull.appSecretName" . }}
      key: encryption-keys
{{- end }}
{{- end -}}

{{/* The key file volume, with encryptionKeys.source=file. */}}
{{- define "sourcegull.keysVolume" -}}
{{- if eq .Values.encryptionKeys.source "file" }}
- name: encryption-keys
  secret:
    secretName: {{ include "sourcegull.appSecretName" . }}
    defaultMode: 0440
    items:
      - key: encryption-keys
        path: encryption-keys
{{- end }}
{{- end -}}

{{- define "sourcegull.keysVolumeMount" -}}
{{- if eq .Values.encryptionKeys.source "file" }}
- name: encryption-keys
  mountPath: /etc/sourcegull/keys
  readOnly: true
{{- end }}
{{- end -}}

{{/*
The configuration files: [value, Secret key, variable] each. A file is used when its value is set,
or with config.existingSecret when config.existingSecretKeys lists its key.
*/}}
{{- define "sourcegull.configFileList" -}}
siteConfig: site-config.json SOURCEGULL_SITE_CONFIG_FILE
globalSettings: global-settings.json SOURCEGULL_GLOBAL_SETTINGS_FILE
resources: resources.json SOURCEGULL_RESOURCES_FILE
{{- end -}}

{{/* "true" when the configuration file `value` is used (dict root, value, key). */}}
{{- define "sourcegull.configFileEnabled" -}}
{{- $c := .root.Values.config -}}
{{- if $c.existingSecret -}}
{{- if has .key $c.existingSecretKeys }}true{{ end -}}
{{- else if not (empty (index $c .value)) -}}
true
{{- end -}}
{{- end -}}

{{/* "true" when any configuration file is used. */}}
{{- define "sourcegull.configEnabled" -}}
{{- range $value, $spec := fromYaml (include "sourcegull.configFileList" .) -}}
{{- $key := index (splitList " " $spec) 0 -}}
{{- if include "sourcegull.configFileEnabled" (dict "root" $ "value" $value "key" $key) }}true{{ end -}}
{{- end -}}
{{- end -}}

{{- define "sourcegull.configSecretName" -}}
{{- default (printf "%s-config" (include "sourcegull.fullname" .)) .Values.config.existingSecret -}}
{{- end -}}

{{- define "sourcegull.configEnv" -}}
{{- range $value, $spec := fromYaml (include "sourcegull.configFileList" .) }}
{{- $parts := splitList " " $spec }}
{{- if include "sourcegull.configFileEnabled" (dict "root" $ "value" $value "key" (index $parts 0)) }}
- name: {{ index $parts 1 }}
  value: /etc/sourcegull/config/{{ index $parts 0 }}
{{- end }}
{{- end }}
{{- end -}}

{{- define "sourcegull.configVolume" -}}
{{- if include "sourcegull.configEnabled" . }}
- name: config
  secret:
    secretName: {{ include "sourcegull.configSecretName" . }}
    defaultMode: 0440
{{- end }}
{{- end -}}

{{- define "sourcegull.configVolumeMount" -}}
{{- if include "sourcegull.configEnabled" . }}
- name: config
  mountPath: /etc/sourcegull/config
  readOnly: true
{{- end }}
{{- end -}}

{{/*
Volumes of the Sourcegull pods: scratch emptyDirs (/tmp, and the data directory unless the component
has a PVC: `dataEmptyDir`), the key file, the configuration files, the S3 CA, and the component's
extraVolumes. Usage: include "sourcegull.runtimeVolumes" (dict "root" $ "values" .Values.web "dataEmptyDir" true)
*/}}
{{- define "sourcegull.runtimeVolumes" -}}
{{- $root := .root -}}
- name: tmp
  {{- include "sourcegull.emptyDir" .values.emptyDirSizeLimit | nindent 2 }}
{{- if .dataEmptyDir }}
- name: data
  {{- include "sourcegull.emptyDir" .values.emptyDirSizeLimit | nindent 2 }}
{{- end }}
{{- with (include "sourcegull.keysVolume" $root | trim) }}
{{ . }}
{{- end }}
{{- with (include "sourcegull.configVolume" $root | trim) }}
{{ . }}
{{- end }}
{{- if and (eq $root.Values.blobstore.mode "s3") $root.Values.blobstore.s3.caConfigMap }}
- name: s3-ca
  configMap:
    name: {{ $root.Values.blobstore.s3.caConfigMap }}
    items:
      - key: {{ $root.Values.blobstore.s3.caConfigMapKey }}
        path: {{ $root.Values.blobstore.s3.caConfigMapKey }}
{{- end }}
{{- with .values.extraVolumes }}
{{ include "sourcegull.tplvalues.render" (dict "value" . "context" $root) }}
{{- end }}
{{- end -}}

{{- define "sourcegull.runtimeVolumeMounts" -}}
{{- $root := .root -}}
- name: tmp
  mountPath: /tmp
- name: data
  mountPath: /var/opt/sourcegull
{{- with (include "sourcegull.keysVolumeMount" $root | trim) }}
{{ . }}
{{- end }}
{{- with (include "sourcegull.configVolumeMount" $root | trim) }}
{{ . }}
{{- end }}
{{- if and (eq $root.Values.blobstore.mode "s3") $root.Values.blobstore.s3.caConfigMap }}
- name: s3-ca
  mountPath: /etc/sourcegull/s3-ca
  readOnly: true
{{- end }}
{{- with .values.extraVolumeMounts }}
{{ include "sourcegull.tplvalues.render" (dict "value" . "context" $root) }}
{{- end }}
{{- end -}}

{{/*
Init containers of the Sourcegull pods when the database runs in the release: wait for it, then
migrate (see `migrate` in values.yaml).
*/}}
{{- define "sourcegull.migrateInitContainers" -}}
{{- $m := .Values.migrate -}}
{{- if and (include "sourcegull.postgresql.inRelease" .) $m.enabled }}
- name: wait-for-postgresql
  image: {{ include "sourcegull.image" (dict "root" . "image" .Values.postgresql.image "defaultTag" "18") }}
  imagePullPolicy: {{ .Values.postgresql.image.pullPolicy }}
  command:
    - sh
    - -c
    # -U and -d: without them libpq looks up the container's user, which the postgres image does
    # not have for runAsUser, and pg_isready gives up without trying.
    - until pg_isready -q -h {{ include "sourcegull.postgresql.host" . }} -p 5432 -U {{ .Values.postgresql.auth.username | quote }} -d {{ .Values.postgresql.auth.database | quote }}; do echo waiting for postgresql; sleep 2; done
  {{- with (include "sourcegull.securityContext" $m.containerSecurityContext) }}
  securityContext:
    {{- . | nindent 4 }}
  {{- end }}
  resources:
    requests: { cpu: 10m, memory: 16Mi }
    limits: { memory: 64Mi }
- name: migrate
  image: {{ include "sourcegull.sourcegullImage" . }}
  imagePullPolicy: {{ .Values.image.pullPolicy }}
  {{- with $m.command }}
  command: {{- include "sourcegull.tplvalues.render" (dict "value" . "context" $) | nindent 4 }}
  {{- end }}
  args: ["migrate", "up"]
  env:
    {{- include "sourcegull.env" . | trim | nindent 4 }}
    {{- with (include "sourcegull.extraEnv" (dict "root" . "values" $m "global" true) | trim) }}{{ . | nindent 4 }}{{ end }}
  {{- with (include "sourcegull.envFrom" (dict "root" . "values" $m "global" true) | trim) }}{{ . | nindent 2 }}{{ end }}
  {{- with (include "sourcegull.securityContext" $m.containerSecurityContext) }}
  securityContext:
    {{- . | nindent 4 }}
  {{- end }}
  resources:
    {{- toYaml $m.resources | nindent 4 }}
  volumeMounts:
    - name: tmp
      mountPath: /tmp
{{- end }}
{{- end -}}

{{/* Namespace of executor job pods. */}}
{{- define "sourcegull.executor.namespace" -}}
{{- default .Release.Namespace .Values.executor.jobs.namespace -}}
{{- end -}}

{{/* The web role's URL for executors. */}}
{{- define "sourcegull.executor.frontendURL" -}}
{{- if .Values.executor.frontendURL -}}
{{- .Values.executor.frontendURL -}}
{{- else -}}
{{- printf "http://%s.%s.svc:%d" (include "sourcegull.componentName" (dict "root" . "component" "web")) .Release.Namespace (int .Values.web.service.ports.http) -}}
{{- end -}}
{{- end -}}
