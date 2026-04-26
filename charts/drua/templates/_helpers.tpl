{{/*
Create a default fully qualified app name.
We truncate at 63 chars because some Kubernetes name fields are limited to this (by the DNS naming spec).
If release name contain chart name it will be used as a full name.
*/}}
{{- define "drua.fullname" -}}
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
{{- define "drua.postgresMcp.fullname" -}}
{{- printf "%s-postgres-mcp" .Release.Name | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{/*
Postgres MCP secret name: defaults to the main drua secret (which already
holds `pg-con`). Allows override via .Values.drua.postgresMcp.databaseUrlSecret.name.
*/}}
{{- define "drua.postgresMcp.secretName" -}}
{{- if .Values.drua.postgresMcp.databaseUrlSecret.name -}}
{{- .Values.drua.postgresMcp.databaseUrlSecret.name -}}
{{- else -}}
{{- template "drua.fullname" . -}}
{{- end -}}
{{- end -}}

{{/*
Sandbox namespace: use .Values.sandbox.namespace if set, otherwise .Release.Namespace
*/}}
{{- define "drua.sandboxNamespace" -}}
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
{{- define "drua.controllerNamespace" -}}
{{- if .Values.sandbox.controllerNamespace -}}
{{- .Values.sandbox.controllerNamespace -}}
{{- else -}}
agent-sandbox-system
{{- end -}}
{{- end -}}
