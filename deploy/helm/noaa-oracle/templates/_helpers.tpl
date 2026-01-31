{{/*
Expand the name of the chart.
*/}}
{{- define "noaa-oracle.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/*
Create a default fully qualified app name.
*/}}
{{- define "noaa-oracle.fullname" -}}
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

{{/*
Create chart name and version as used by the chart label.
*/}}
{{- define "noaa-oracle.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/*
Common labels
*/}}
{{- define "noaa-oracle.labels" -}}
helm.sh/chart: {{ include "noaa-oracle.chart" . }}
{{ include "noaa-oracle.selectorLabels" . }}
{{- if .Chart.AppVersion }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
{{- end }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end }}

{{/*
Selector labels
*/}}
{{- define "noaa-oracle.selectorLabels" -}}
app.kubernetes.io/name: {{ include "noaa-oracle.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

{{/*
Create the name of the service account to use
*/}}
{{- define "noaa-oracle.serviceAccountName" -}}
{{- if .Values.serviceAccount.create }}
{{- default (include "noaa-oracle.fullname" .) .Values.serviceAccount.name }}
{{- else }}
{{- default "default" .Values.serviceAccount.name }}
{{- end }}
{{- end }}

{{/*
Return the image name
*/}}
{{- define "noaa-oracle.image" -}}
{{- $tag := default .Chart.AppVersion .Values.image.tag -}}
{{- printf "%s:%s" .Values.image.repository $tag -}}
{{- end }}

{{/*
Service selector labels - uses activeSlot when blue/green is enabled
*/}}
{{- define "noaa-oracle.serviceSelectorLabels" -}}
{{ include "noaa-oracle.selectorLabels" . }}
{{- if .Values.blueGreen.enabled }}
app.kubernetes.io/slot: {{ .Values.blueGreen.activeSlot }}
{{- end }}
{{- end }}
