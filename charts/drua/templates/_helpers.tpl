{{/*
Create a default fully qualified app name.
We truncate at 63 chars because some Kubernetes name fields are limited to this (by the DNS naming spec).
If release name contain chart name it will be used as a full name.
*/}}
{{- define "galoyAgents.fullname" -}}
{{- if .Values.fullnameOverride }}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- $name := default .Chart.Name .Values.nameOverride }}
{{- if contains $name .Release.Name }}
{{- .Release.Name | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- printf "%s" $name | trunc 63 | trimSuffix "-" }}
{{- end }}
{{- end }}
{{- end }}

{{/*
Postgres MCP fullname: <release>-postgres-mcp
*/}}
{{- define "galoyAgents.postgresMcp.fullname" -}}
{{- printf "%s-postgres-mcp" .Release.Name | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{/*
Postgres MCP secret name: defaults to the main galoy-agents secret (which already
holds `pg-con`). Allows override via .Values.galoyAgents.postgresMcp.databaseUrlSecret.name.
*/}}
{{- define "galoyAgents.postgresMcp.secretName" -}}
{{- if .Values.galoyAgents.postgresMcp.databaseUrlSecret.name -}}
{{- .Values.galoyAgents.postgresMcp.databaseUrlSecret.name -}}
{{- else -}}
{{- template "galoyAgents.fullname" . -}}
{{- end -}}
{{- end -}}

{{/*
Sandbox namespace: use .Values.sandbox.namespace if set, otherwise .Release.Namespace
*/}}
{{- define "galoyAgents.sandboxNamespace" -}}
{{- if .Values.sandbox.namespace -}}
{{- .Values.sandbox.namespace -}}
{{- else -}}
{{- .Release.Namespace -}}
{{- end -}}
{{- end -}}

{{/*
Sandbox controller namespace: use .Values.sandbox.controllerNamespace if set,
otherwise "agent-sandbox-system" (upstream default).
*/}}
{{- define "galoyAgents.controllerNamespace" -}}
{{- if .Values.sandbox.controllerNamespace -}}
{{- .Values.sandbox.controllerNamespace -}}
{{- else -}}
agent-sandbox-system
{{- end -}}
{{- end -}}

{{/*
Sandbox-local Attic fullname: <app>-attic
*/}}
{{- define "galoyAgents.attic.fullname" -}}
{{- printf "%s-attic" (include "galoyAgents.fullname" .) | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{/*
Attic config secret. Defaults to <app>-attic-config unless overridden.
*/}}
{{- define "galoyAgents.attic.configSecretName" -}}
{{- if .Values.sandbox.attic.configSecret.name -}}
{{- .Values.sandbox.attic.configSecret.name -}}
{{- else -}}
{{- printf "%s-config" (include "galoyAgents.attic.fullname" .) | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}

{{/*
Internal Attic binary cache URL for sandbox NIX_CONFIG.
*/}}
{{- define "galoyAgents.attic.cacheUrl" -}}
{{- printf "http://%s.%s.svc.cluster.local:%v/%s" (include "galoyAgents.attic.fullname" .) (include "galoyAgents.sandboxNamespace" .) .Values.sandbox.attic.service.port .Values.sandbox.attic.cacheName -}}
{{- end -}}
