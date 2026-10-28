"""Generates values.schema.json from values.yaml: types from the defaults, closed objects except
free-form maps, plus the constraints in RULES. Usage: genschema.py values.yaml out.json"""
import json
import re
import sys

import yaml

values = yaml.safe_load(open(sys.argv[1]))

# Keys whose objects are free-form (any keys), wherever they appear.
OPEN_KEYS = {
    "podLabels", "podAnnotations", "nodeSelector", "affinity", "annotations", "labels",
    "commonLabels", "commonAnnotations", "resources", "requests", "limits", "siteConfig",
    "globalSettings", "lifecycleHooks", "customStartupProbe", "customLivenessProbe",
    "customReadinessProbe", "seLinuxOptions", "capabilities", "seccompProfile", "updateStrategy",
    "walStorage", "backup", "extraSpec", "matchLabels",
}
# Paths of free-form objects.
OPEN_PATHS = {"postgresql.config", "config.resources"}
# Values that are an integer or a string (a percentage, or "" for unset).
INT_OR_EMPTY = {"minAvailable", "maxUnavailable"}

PORT = {"type": "integer", "minimum": 1, "maximum": 65535}
PULL = {"enum": ["Always", "IfNotPresent", "Never"]}
PRESET = {"enum": ["", "soft", "hard"]}
DURATION = {"type": "string", "pattern": "^([0-9]+(ms|s|m|h))*$"}

# Constraints by path; `*` matches one key.
RULES = {
    "externalURL": {"type": "string", "pattern": "^https?://[^/]+"},
    "logFormat": {"enum": ["json", "pretty"]},
    "encryptionKeys.source": {"enum": ["file", "env", "external"]},
    "config.existingSecretKeys": {"type": "array", "items": {"enum": ["site-config.json", "global-settings.json", "resources.json"]}},
    "leader.backend": {"enum": ["kubernetes", "postgres"]},
    "leader.leaseDurationSeconds": {"type": "integer", "minimum": 6},
    "rateLimit.backend": {"enum": ["postgres", "memory"]},
    "*.image.pullPolicy": PULL, "image.pullPolicy": PULL, "tests.smoke.image.pullPolicy": PULL,
    "*.image.repository": {"type": "string", "minLength": 1},
    "image.repository": {"type": "string", "minLength": 1},
    "tests.smoke.image.repository": {"type": "string", "minLength": 1},
    "image.tag": {"type": ["string", "number"]}, "*.image.tag": {"type": ["string", "number"]},
    "tests.smoke.image.tag": {"type": ["string", "number"]},
    "*.replicaCount": {"type": "integer", "minimum": 0},
    "repo.replicaCount": {"type": "integer", "minimum": 1},
    "*.autoscaling.hpa.minReplicas": {"type": "integer", "minimum": 1},
    "*.autoscaling.hpa.maxReplicas": {"type": "integer", "minimum": 1},
    "*.autoscaling.hpa.targetCPU": {"type": ["integer", "string"], "pattern": "^([1-9][0-9]*)?$", "minimum": 1},
    "*.autoscaling.hpa.targetMemory": {"type": ["integer", "string"], "pattern": "^([1-9][0-9]*)?$", "minimum": 1},
    "*.podAffinityPreset": PRESET, "*.podAntiAffinityPreset": PRESET,
    "*.nodeAffinityPreset.type": PRESET,
    "*.podSecurityContext.fsGroup": {"type": "integer", "minimum": 0},
    "*.podSecurityContext.fsGroupChangePolicy": {"enum": ["Always", "OnRootMismatch"]},
    "*.containerSecurityContext.runAsUser": {"type": "integer", "minimum": 0},
    "*.containerSecurityContext.runAsGroup": {"type": "integer", "minimum": 0},
    "*.persistence.size": {"type": "string", "minLength": 1},
    "*.persistence.accessModes": {"type": "array", "items": {"enum": ["ReadWriteOnce", "ReadWriteOncePod", "ReadWriteMany"]}},
    "*.persistentVolumeClaimRetentionPolicy.whenScaled": {"enum": ["Retain", "Delete"]},
    "*.persistentVolumeClaimRetentionPolicy.whenDeleted": {"enum": ["Retain", "Delete"]},
    "repo.podManagementPolicy": {"enum": ["Parallel", "OrderedReady"]},
    "web.service.type": {"enum": ["ClusterIP", "NodePort", "LoadBalancer"]},
    "web.service.ports.http": PORT, "web.service.ports.internal": PORT,
    "web.service.nodePorts.http": {"type": ["integer", "string"], "pattern": "^([0-9]+)?$"},
    "web.service.externalTrafficPolicy": {"enum": ["Cluster", "Local"]},
    "web.service.sessionAffinity": {"enum": ["None", "ClientIP"]},
    "repo.service.ports.grpc": PORT, "repo.service.ports.internal": PORT,
    "blobstore.service.ports.internal": PORT,
    "worker.autoscaling.keda.trigger": {"enum": ["postgresql", "prometheus"]},
    "worker.autoscaling.keda.prometheus.threshold": {"type": ["integer", "string"], "pattern": "^([1-9][0-9]*)?$", "minimum": 1},
    "worker.autoscaling.keda.minReplicas": {"type": "integer", "minimum": 1},
    "worker.autoscaling.keda.maxReplicas": {"type": "integer", "minimum": 1},
    "worker.autoscaling.keda.targetJobsPerReplica": {"type": "integer", "minimum": 1},
    "worker.autoscaling.keda.pollingInterval": {"type": "integer", "minimum": 1},
    "worker.autoscaling.keda.cooldownPeriod": {"type": "integer", "minimum": 0},
    "worker.autoscaling.keda.scaleDownStabilizationSeconds": {"type": "integer", "minimum": 0},
    "worker.autoscaling.keda.excludeQueues": {"type": "array", "items": {"type": "string", "pattern": "^[a-z_][a-z0-9_]*$"}},
    "migrate.backoffLimit": {"type": "integer", "minimum": 0},
    "migrate.activeDeadlineSeconds": {"type": "integer", "minimum": 1},
    "migrate.ttlSecondsAfterFinished": {"type": "integer", "minimum": 0},
    "postgresql.mode": {"enum": ["bundled", "cnpg", "external"]},
    "postgresql.auth.username": {"type": "string", "pattern": "^[A-Za-z_][A-Za-z0-9_]*$"},
    "postgresql.auth.database": {"type": "string", "pattern": "^[A-Za-z_][A-Za-z0-9_]*$"},
    "postgresql.auth.existingSecretPasswordKey": {"type": "string", "minLength": 1},
    "postgresql.cnpg.instances": {"type": "integer", "minimum": 1},
    "postgresql.cnpg.primaryUpdateStrategy": {"enum": ["unsupervised", "supervised"]},
    "externalDatabase.existingSecretUrlKey": {"type": "string", "minLength": 1},
    "blobstore.mode": {"enum": ["builtin", "s3"]},
    "blobstore.s3.bucket": {"type": "string", "pattern": "^[a-z0-9][a-z0-9.-]*[a-z0-9]$|^$"},
    "blobstore.s3.prefix": {"type": "string", "pattern": "^[A-Za-z0-9_/-][A-Za-z0-9._/-]*$|^$"},
    "blobstore.s3.endpoint": {"type": "string", "pattern": "^https?://|^$"},
    "blobstore.s3.region": {"type": "string", "minLength": 1},
    "blobstore.s3.existingSecretAccessKeyIdKey": {"type": "string", "minLength": 1},
    "blobstore.s3.existingSecretSecretAccessKeyKey": {"type": "string", "minLength": 1},
    "blobstore.s3.caConfigMapKey": {"type": "string", "minLength": 1},
    "executor.queues": {"type": "array", "items": {"type": "string", "enum": ["codeintel", "batches"]}},
    "executor.maxConcurrentJobs": {"type": "integer", "minimum": 1},
    "executor.existingSecretTokenKey": {"type": "string", "minLength": 1},
    "executor.jobs.deadlineSeconds": {"type": "integer", "minimum": 60},
    "executor.jobs.startTimeoutSeconds": {"type": "integer", "minimum": 10},
    "ingress.pathType": {"enum": ["Prefix", "Exact", "ImplementationSpecific"]},
    "metrics.serviceMonitor.interval": DURATION,
    "metrics.serviceMonitor.scrapeTimeout": DURATION,
    "tests.smoke.username": {"type": "string", "minLength": 1, "pattern": "^[A-Za-z0-9][A-Za-z0-9._-]*$"},
    "tests.smoke.searchQuery": {"type": "string", "minLength": 1},
}
# Probe settings.
for probe in ("startupProbe", "livenessProbe", "readinessProbe"):
    for key, minimum in (("initialDelaySeconds", 0), ("periodSeconds", 1), ("timeoutSeconds", 1),
                         ("failureThreshold", 1), ("successThreshold", 1)):
        RULES[f"*.{probe}.{key}"] = {"type": "integer", "minimum": minimum}
RULES["*.terminationGracePeriodSeconds"] = {"type": "integer", "minimum": 0}


def rule_for(path):
    if path in RULES:
        return RULES[path]
    parts = path.split(".")
    for pattern, rule in RULES.items():
        p = pattern.split(".")
        if len(p) == len(parts) and all(a == "*" or a == b for a, b in zip(p, parts)):
            return rule
    return None


def schema(value, path, key):
    rule = rule_for(path)
    if isinstance(value, dict):
        if key in OPEN_KEYS or path in OPEN_PATHS or not value:
            return {"type": "object"}
        props = {k: schema(v, f"{path}.{k}" if path else k, k) for k, v in value.items()}
        return {"type": "object", "additionalProperties": False, "properties": props}
    if rule:
        return rule
    if key in INT_OR_EMPTY:
        return {"type": ["integer", "string"]}
    if isinstance(value, bool):
        return {"type": "boolean"}
    if isinstance(value, int):
        return {"type": "integer"}
    if isinstance(value, float):
        return {"type": "number"}
    if isinstance(value, list):
        if value and all(isinstance(v, str) for v in value):
            return {"type": "array", "items": {"type": "string"}}
        return {"type": "array"}
    return {"type": "string"}


out = {"$schema": "http://json-schema.org/draft-07/schema#", "title": "Sourcegull Helm chart values"}
out.update(schema(values, "", ""))
out["required"] = ["externalURL"]
# Pull secrets: names or { name }.
pull = {"type": "array", "items": {"anyOf": [{"type": "string"}, {"type": "object", "required": ["name"]}]}}
out["properties"]["global"]["properties"]["imagePullSecrets"] = pull
for img in (out["properties"]["image"], out["properties"]["postgresql"]["properties"]["image"]):
    img["properties"]["pullSecrets"] = pull
# Every rule must have matched a values path.
paths = set()


def collect(value, path):
    paths.add(path)
    if isinstance(value, dict):
        for k, v in value.items():
            collect(v, f"{path}.{k}" if path else k)


collect(values, "")
for pattern in RULES:
    p = pattern.split(".")
    if not any(len(x.split(".")) == len(p) and all(a == "*" or a == b for a, b in zip(p, x.split("."))) for x in paths):
        sys.exit(f"rule matches no value: {pattern}")
json.dump(out, open(sys.argv[2], "w"), indent=2)
open(sys.argv[2], "a").write("\n")
