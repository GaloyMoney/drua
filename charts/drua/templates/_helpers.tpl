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
In-cluster URL for the git-proxy served by this drua-server release.
Used as the default for `sandbox.gitProxyUrl` so ops don't have to
hand-write the DNS name + port. Shape mirrors
`galoyAgents.postgresMcp` URL construction.
*/}}
{{- define "galoyAgents.gitProxyUrl" -}}
http://{{ template "galoyAgents.fullname" . }}.{{ .Release.Namespace }}.svc.cluster.local:{{ .Values.galoyAgents.server.service.port }}/git
{{- end -}}

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
Sandbox Nix netrc Secret. Used for authenticated private binary caches.
*/}}
{{- define "galoyAgents.sandbox.nixNetrcSecretName" -}}
{{- if .Values.sandbox.nixNetrc.secret.name -}}
{{- .Values.sandbox.nixNetrc.secret.name -}}
{{- else -}}
{{- printf "%s-sandbox-nix-netrc" (include "galoyAgents.fullname" .) | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}

{{/*
Sandbox Nix cache proxy names and service URL.
*/}}
{{- define "galoyAgents.sandbox.nixCacheProxy.fullname" -}}
{{- printf "%s-sandbox-nix-cache-proxy" (include "galoyAgents.fullname" .) | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "galoyAgents.sandbox.nixCacheProxy.url" -}}
http://{{ include "galoyAgents.sandbox.nixCacheProxy.fullname" . }}.{{ .Release.Namespace }}.svc.cluster.local:{{ .Values.sandbox.nixCacheProxy.service.port }}
{{- end -}}

{{/*
Sandbox NIX_CONFIG contents for configured remote substituters.
*/}}
{{- define "galoyAgents.sandbox.nixSubstituterUrls" -}}
{{- if and .Values.sandbox.nixCacheProxy.enabled (not (gt (len .Values.sandbox.nixCacheProxy.upstreams) 0)) -}}
{{- fail "sandbox.nixCacheProxy.upstreams must not be empty when sandbox.nixCacheProxy.enabled=true" -}}
{{- end -}}
{{- if .Values.sandbox.nixCacheProxy.enabled -}}
{{- $root := . -}}
{{- range $i, $upstream := .Values.sandbox.nixCacheProxy.upstreams -}}
{{- $name := required "sandbox.nixCacheProxy.upstreams[].name is required" $upstream.name -}}
{{- if $i }} {{ end -}}{{ include "galoyAgents.sandbox.nixCacheProxy.url" $root }}/{{ $name }}/
{{- end -}}
{{- else -}}
{{- range $i, $substituter := .Values.sandbox.nixSubstituters -}}
{{- if $i }} {{ end -}}{{ $substituter.url -}}
{{- end -}}
{{- end -}}
{{- end -}}

{{- define "galoyAgents.sandbox.nixPublicKeys" -}}
{{- $first := true -}}
{{- if .Values.sandbox.nixCacheProxy.enabled -}}
{{- range $substituter := .Values.sandbox.nixCacheProxy.upstreams -}}
{{- if $substituter.publicKey -}}
{{- if not $first }} {{ end -}}{{ $substituter.publicKey -}}
{{- $first = false -}}
{{- end -}}
{{- end -}}
{{- else -}}
{{- range $substituter := .Values.sandbox.nixSubstituters -}}
{{- if $substituter.publicKey -}}
{{- if not $first }} {{ end -}}{{ $substituter.publicKey -}}
{{- $first = false -}}
{{- end -}}
{{- end -}}
{{- if not $first }} {{ end -}}cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY=
{{- end -}}
{{- end -}}

{{- define "galoyAgents.sandbox.nixConfig" -}}
substituters ={{- with (include "galoyAgents.sandbox.nixSubstituterUrls" .) }} {{ . }}{{- end }}{{- if not .Values.sandbox.nixCacheProxy.enabled }} https://cache.nixos.org/{{- end }}
trusted-public-keys ={{- with (include "galoyAgents.sandbox.nixPublicKeys" .) }} {{ . }}{{- end }}
{{- if and .Values.sandbox.nixNetrc.enabled (not .Values.sandbox.nixCacheProxy.enabled) }}
netrc-file = {{ .Values.sandbox.nixNetrc.mountPath }}
{{- end }}
{{- end -}}
