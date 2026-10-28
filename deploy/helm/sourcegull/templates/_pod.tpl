{{/*
Pod and container building blocks shared by the components. Most take
(dict "root" $ "values" .Values.<component> ...), where `values` is the component's section.
*/}}

{{/* A security context section without `enabled` and empty entries; nothing when disabled. */}}
{{- define "sourcegull.securityContext" -}}
{{- if .enabled -}}
{{- $out := dict -}}
{{- range $key, $value := omit . "enabled" -}}
{{- if or (kindIs "bool" $value) (kindIs "float64" $value) (kindIs "int64" $value) (kindIs "int" $value) (not (empty $value)) -}}
{{- $_ := set $out $key $value -}}
{{- end -}}
{{- end -}}
{{- toYaml $out -}}
{{- end -}}
{{- end -}}

{{/* An emptyDir volume source, with an optional size limit. Usage: include "sourcegull.emptyDir" "2Gi" */}}
{{- define "sourcegull.emptyDir" -}}
{{- if . -}}
emptyDir:
  sizeLimit: {{ . | quote }}
{{- else -}}
emptyDir: {}
{{- end -}}
{{- end -}}

{{/*
Affinity: the component's `affinity`, or one built from podAffinityPreset, podAntiAffinityPreset
(soft or hard, over the component's pods per Kubernetes node) and nodeAffinityPreset.
Usage: include "sourcegull.affinity" (dict "root" $ "values" .Values.web "component" "web")
*/}}
{{- define "sourcegull.affinity" -}}
{{- $v := .values -}}
{{- if $v.affinity -}}
{{- include "sourcegull.tplvalues.render" (dict "value" $v.affinity "context" .root) -}}
{{- else -}}
{{- $out := dict -}}
{{- $term := dict "topologyKey" "kubernetes.io/hostname" "labelSelector" (dict "matchLabels" (fromYaml (include "sourcegull.selectorLabels" .))) -}}
{{- range $kind, $preset := dict "podAffinity" (default "" $v.podAffinityPreset) "podAntiAffinity" (default "" $v.podAntiAffinityPreset) -}}
{{- if eq $preset "soft" -}}
{{- $_ := set $out $kind (dict "preferredDuringSchedulingIgnoredDuringExecution" (list (dict "weight" 100 "podAffinityTerm" $term))) -}}
{{- else if eq $preset "hard" -}}
{{- $_ := set $out $kind (dict "requiredDuringSchedulingIgnoredDuringExecution" (list $term)) -}}
{{- end -}}
{{- end -}}
{{- $node := default dict $v.nodeAffinityPreset -}}
{{- if and $node.type $node.key -}}
{{- $match := dict "matchExpressions" (list (dict "key" $node.key "operator" "In" "values" $node.values)) -}}
{{- if eq $node.type "soft" -}}
{{- $_ := set $out "nodeAffinity" (dict "preferredDuringSchedulingIgnoredDuringExecution" (list (dict "weight" 1 "preference" $match))) -}}
{{- else if eq $node.type "hard" -}}
{{- $_ := set $out "nodeAffinity" (dict "requiredDuringSchedulingIgnoredDuringExecution" (dict "nodeSelectorTerms" (list $match))) -}}
{{- end -}}
{{- end -}}
{{- with $out }}{{ toYaml . }}{{ end -}}
{{- end -}}
{{- end -}}

{{/*
Pod spec scheduling fields: hostAliases, priorityClassName, schedulerName, nodeSelector,
tolerations, affinity and topologySpreadConstraints.
Usage: include "sourcegull.podScheduling" (dict "root" $ "values" .Values.web "component" "web")
*/}}
{{- define "sourcegull.podScheduling" -}}
{{- $root := .root -}}
{{- $v := .values -}}
{{- with $v.hostAliases }}
hostAliases:
  {{- include "sourcegull.tplvalues.render" (dict "value" . "context" $root) | nindent 2 }}
{{- end }}
{{- with $v.priorityClassName }}
priorityClassName: {{ . | quote }}
{{- end }}
{{- with $v.schedulerName }}
schedulerName: {{ . | quote }}
{{- end }}
{{- with $v.nodeSelector }}
nodeSelector:
  {{- include "sourcegull.tplvalues.render" (dict "value" . "context" $root) | nindent 2 }}
{{- end }}
{{- with $v.tolerations }}
tolerations:
  {{- include "sourcegull.tplvalues.render" (dict "value" . "context" $root) | nindent 2 }}
{{- end }}
{{- with (include "sourcegull.affinity" $) }}
affinity:
  {{- . | nindent 2 }}
{{- end }}
{{- with $v.topologySpreadConstraints }}
topologySpreadConstraints:
  {{- include "sourcegull.tplvalues.render" (dict "value" . "context" $root) | nindent 2 }}
{{- end }}
{{- end -}}

{{/* The pod's `securityContext:` block (podSecurityContext), when enabled. */}}
{{- define "sourcegull.podSecurityContext" -}}
{{- with (include "sourcegull.securityContext" .values.podSecurityContext) }}
securityContext:
  {{- . | nindent 2 }}
{{- end -}}
{{- end -}}

{{/* The container's `securityContext:` block (containerSecurityContext), when enabled. */}}
{{- define "sourcegull.containerSecurityContext" -}}
{{- with (include "sourcegull.securityContext" .values.containerSecurityContext) }}
securityContext:
  {{- . | nindent 2 }}
{{- end -}}
{{- end -}}

{{/* The container's `command:` and `args:` (the component's, else `defaultArgs`). */}}
{{- define "sourcegull.commandArgs" -}}
{{- with .values.command }}
command:
  {{- include "sourcegull.tplvalues.render" (dict "value" . "context" $.root) | nindent 2 }}
{{- end }}
{{- if .values.args }}
args:
  {{- include "sourcegull.tplvalues.render" (dict "value" .values.args "context" .root) | nindent 2 }}
{{- else if .defaultArgs }}
args: {{ toJson .defaultArgs }}
{{- end }}
{{- end -}}

{{/*
HTTP probes on the internal listener: startup and liveness on /healthz, readiness on /readyz;
custom*Probe replaces a probe.
*/}}
{{- define "sourcegull.probes" -}}
{{- $v := .values -}}
{{- range $name := list "startupProbe" "livenessProbe" "readinessProbe" }}
{{- $custom := index $v (printf "custom%s" (title $name)) }}
{{- $probe := index $v $name }}
{{- if $custom }}
{{ $name }}:
  {{- include "sourcegull.tplvalues.render" (dict "value" $custom "context" $.root) | nindent 2 }}
{{- else if $probe.enabled }}
{{ $name }}:
  httpGet:
    path: {{ ternary "/readyz" "/healthz" (eq $name "readinessProbe") }}
    port: internal
  {{- toYaml (omit $probe "enabled") | nindent 2 }}
{{- end }}
{{- end }}
{{- end -}}

{{/*
The container's `lifecycle:`: lifecycleHooks, or with `preStopPause` a 5 s preStop pause so the
pod keeps serving while its endpoint is removed from Services (the process closes its listeners
as soon as it gets SIGTERM).
*/}}
{{- define "sourcegull.lifecycle" -}}
{{- if .values.lifecycleHooks }}
lifecycle:
  {{- include "sourcegull.tplvalues.render" (dict "value" .values.lifecycleHooks "context" .root) | nindent 2 }}
{{- else if .preStopPause }}
lifecycle:
  preStop:
    exec:
      command: ["sleep", "5"]
{{- end }}
{{- end -}}

{{/* Extra env entries: the top-level extraEnvVars (with `global`), then the component's. */}}
{{- define "sourcegull.extraEnv" -}}
{{- $env := list -}}
{{- if .global }}{{ $env = concat $env (default list .root.Values.extraEnvVars) }}{{ end -}}
{{- $env = concat $env (default list .values.extraEnvVars) -}}
{{- with $env }}
{{- include "sourcegull.tplvalues.render" (dict "value" . "context" $.root) }}
{{- end -}}
{{- end -}}

{{/* The container's `envFrom:`: extraEnvVarsCM and extraEnvVarsSecret (top-level ones with `global`). */}}
{{- define "sourcegull.envFrom" -}}
{{- $maps := list .values.extraEnvVarsCM -}}
{{- $secrets := list .values.extraEnvVarsSecret -}}
{{- if .global -}}
{{- $maps = prepend $maps .root.Values.extraEnvVarsCM -}}
{{- $secrets = prepend $secrets .root.Values.extraEnvVarsSecret -}}
{{- end -}}
{{- $maps = compact $maps -}}
{{- $secrets = compact $secrets -}}
{{- if or $maps $secrets }}
envFrom:
{{- range $maps }}
  - configMapRef:
      name: {{ tpl . $.root }}
{{- end }}
{{- range $secrets }}
  - secretRef:
      name: {{ tpl . $.root }}
{{- end }}
{{- end }}
{{- end -}}

{{/* The component's sidecars, after the main container. */}}
{{- define "sourcegull.sidecars" -}}
{{- with .values.sidecars }}
{{ include "sourcegull.tplvalues.render" (dict "value" . "context" $.root) }}
{{- end -}}
{{- end -}}

{{/* The pod's `initContainers:`: `builtin` (rendered init containers) then the component's own. */}}
{{- define "sourcegull.initContainers" -}}
{{- $builtin := default "" .builtin | trim -}}
{{- if or $builtin .values.initContainers }}
initContainers:
{{- with $builtin }}
  {{- . | nindent 2 }}
{{- end }}
{{- with .values.initContainers }}
  {{- include "sourcegull.tplvalues.render" (dict "value" . "context" $.root) | nindent 2 }}
{{- end }}
{{- end }}
{{- end -}}

{{/* StorageClass of a PVC: the persistence's, else global.defaultStorageClass; "-" means "" (no dynamic provisioning). */}}
{{- define "sourcegull.storageClass" -}}
{{- $class := default .root.Values.global.defaultStorageClass .persistence.storageClass -}}
{{- if $class -}}
{{- if eq $class "-" }}
storageClassName: ""
{{- else }}
storageClassName: {{ $class | quote }}
{{- end }}
{{- end -}}
{{- end -}}

{{/* The `data` volumeClaimTemplate of a StatefulSet. Usage: include "sourcegull.volumeClaimTemplate" (dict "root" $ "persistence" .Values.repo.persistence "component" "repo") */}}
{{- define "sourcegull.volumeClaimTemplate" -}}
{{- $p := .persistence -}}
- metadata:
    name: data
    labels:
      {{- toYaml (merge (fromYaml (include "sourcegull.selectorLabels" .)) (default dict $p.labels)) | nindent 6 }}
    {{- with $p.annotations }}
    annotations:
      {{- toYaml . | nindent 6 }}
    {{- end }}
  spec:
    accessModes:
      {{- toYaml $p.accessModes | nindent 6 }}
    {{- with (include "sourcegull.storageClass" . | trim) }}{{ . | nindent 4 }}{{ end }}
    resources:
      requests:
        storage: {{ $p.size | quote }}
{{- end -}}

{{/* A StatefulSet's persistentVolumeClaimRetentionPolicy, when enabled. */}}
{{- define "sourcegull.pvcRetentionPolicy" -}}
{{- with .persistentVolumeClaimRetentionPolicy }}
{{- if .enabled }}
persistentVolumeClaimRetentionPolicy:
  whenScaled: {{ .whenScaled }}
  whenDeleted: {{ .whenDeleted }}
{{- end }}
{{- end }}
{{- end -}}

{{/* A PodDisruptionBudget: minAvailable when set, else maxUnavailable (default 1). */}}
{{- define "sourcegull.pdb" -}}
{{- $pdb := .values.pdb -}}
apiVersion: policy/v1
kind: PodDisruptionBudget
metadata:
  name: {{ include "sourcegull.componentName" . }}
  labels:
    {{- include "sourcegull.componentLabels" . | nindent 4 }}
  {{- with (include "sourcegull.annotations" (dict "root" .root) | trim) }}{{ . | nindent 2 }}{{ end }}
spec:
  {{- if $pdb.minAvailable }}
  minAvailable: {{ $pdb.minAvailable }}
  {{- else }}
  maxUnavailable: {{ default 1 $pdb.maxUnavailable }}
  {{- end }}
  selector:
    matchLabels:
      {{- include "sourcegull.selectorLabels" . | nindent 6 }}
{{- end -}}

{{/* A CPU (and memory) HorizontalPodAutoscaler for a Deployment, from autoscaling.hpa. */}}
{{- define "sourcegull.hpa" -}}
{{- $hpa := .values.autoscaling.hpa -}}
apiVersion: autoscaling/v2
kind: HorizontalPodAutoscaler
metadata:
  name: {{ include "sourcegull.componentName" . }}
  labels:
    {{- include "sourcegull.componentLabels" . | nindent 4 }}
  {{- with (include "sourcegull.annotations" (dict "root" .root) | trim) }}{{ . | nindent 2 }}{{ end }}
spec:
  scaleTargetRef:
    apiVersion: apps/v1
    kind: Deployment
    name: {{ include "sourcegull.componentName" . }}
  minReplicas: {{ $hpa.minReplicas }}
  maxReplicas: {{ $hpa.maxReplicas }}
  metrics:
    {{- with $hpa.targetCPU }}
    - type: Resource
      resource:
        name: cpu
        target:
          type: Utilization
          averageUtilization: {{ . }}
    {{- end }}
    {{- with $hpa.targetMemory }}
    - type: Resource
      resource:
        name: memory
        target:
          type: Utilization
          averageUtilization: {{ . }}
    {{- end }}
{{- end -}}
