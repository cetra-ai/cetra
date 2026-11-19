{{/* Names and labels of the cetra chart (D-N-10). */}}

{{- define "cetra.name" -}}
{{- .Chart.Name | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "cetra.fullname" -}}
{{- if contains .Chart.Name .Release.Name -}}
{{- .Release.Name | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- printf "%s-%s" .Release.Name .Chart.Name | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}

{{- define "cetra.selectorLabels" -}}
app.kubernetes.io/name: {{ include "cetra.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{- define "cetra.labels" -}}
{{ include "cetra.selectorLabels" . }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{- end -}}

{{- define "cetra.image" -}}
{{- printf "%s:%s" .Values.image.repository (default .Chart.AppVersion .Values.image.tag) -}}
{{- end -}}

{{- define "cetra.objectStore" -}}
{{- if .Values.s3.prefix -}}
{{- printf "s3://%s/%s" .Values.s3.bucket (trimAll "/" .Values.s3.prefix) -}}
{{- else -}}
{{- printf "s3://%s" .Values.s3.bucket -}}
{{- end -}}
{{- end -}}
