{{/* Names and labels of the llmobs chart (D-N-10). */}}

{{- define "llmobs.name" -}}
{{- .Chart.Name | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "llmobs.fullname" -}}
{{- if contains .Chart.Name .Release.Name -}}
{{- .Release.Name | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- printf "%s-%s" .Release.Name .Chart.Name | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}

{{- define "llmobs.selectorLabels" -}}
app.kubernetes.io/name: {{ include "llmobs.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{- define "llmobs.labels" -}}
{{ include "llmobs.selectorLabels" . }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{- end -}}

{{- define "llmobs.image" -}}
{{- printf "%s:%s" .Values.image.repository (default .Chart.AppVersion .Values.image.tag) -}}
{{- end -}}

{{- define "llmobs.objectStore" -}}
{{- if .Values.s3.prefix -}}
{{- printf "s3://%s/%s" .Values.s3.bucket (trimAll "/" .Values.s3.prefix) -}}
{{- else -}}
{{- printf "s3://%s" .Values.s3.bucket -}}
{{- end -}}
{{- end -}}
