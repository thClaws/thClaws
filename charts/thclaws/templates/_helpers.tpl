{{/*
Expand the name of the chart.
*/}}
{{- define "thclaws.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" }}
{{- end }}

{{- define "thclaws.fullname" -}}
{{- if .Values.fullnameOverride }}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- $name := default .Chart.Name .Values.nameOverride }}
{{- if contains $name .Release.Name }}
{{- .Release.Name | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- printf "%s-%s" .Release.Name $name | trunc 63 | trimSuffix "-" }}
{{- end }}
{{- end }}
{{- end }}

{{- define "thclaws.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{- end }}

{{- define "thclaws.labels" -}}
helm.sh/chart: {{ include "thclaws.chart" . }}
{{ include "thclaws.selectorLabels" . }}
app.kubernetes.io/version: {{ .Values.image.tag | default .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end }}

{{- define "thclaws.selectorLabels" -}}
app.kubernetes.io/name: {{ include "thclaws.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

{{- define "thclaws.serviceAccountName" -}}
{{- if .Values.serviceAccount.create }}
{{- default (include "thclaws.fullname" .) .Values.serviceAccount.name }}
{{- else }}
{{- default "default" .Values.serviceAccount.name }}
{{- end }}
{{- end }}

{{/* Name of the Secret holding API keys */}}
{{- define "thclaws.apiKeysSecretName" -}}
{{- if .Values.apiKeys.existingSecret }}
{{- .Values.apiKeys.existingSecret }}
{{- else }}
{{- printf "%s-api-keys" (include "thclaws.fullname" .) }}
{{- end }}
{{- end }}

{{/* Name of the Secret holding the HMAC secret */}}
{{- define "thclaws.hmacSecretName" -}}
{{- if .Values.multiTenant.existingSecret }}
{{- .Values.multiTenant.existingSecret }}
{{- else }}
{{- printf "%s-hmac" (include "thclaws.fullname" .) }}
{{- end }}
{{- end }}

{{/* Image tag — falls back to Chart.AppVersion */}}
{{- define "thclaws.imageTag" -}}
{{- .Values.image.tag | default .Chart.AppVersion }}
{{- end }}
