{{/*
Worker queue depth for KEDA: over every pgqueue table (found by its standard
columns, like the admin Background jobs page) except worker.autoscaling.keda.excludeQueues, the
jobs that are queued and due plus the jobs being processed. One SQL statement with no function to
install: query_to_xml runs the per-table count.
*/}}
{{- /*
PromQL for the KEDA `prometheus` trigger: the `sourcegull_queue_jobs` gauge (one series per queue table
and state, exported by every worker process, hence `max` across instances) summed over the queued
and processing jobs of every queue the worker processes. Unlike the SQL query, `queued` also
counts jobs waiting out a retry delay.
*/ -}}
{{- define "sourcegull.worker.queueDepthPromQL" -}}
{{- $excluded := list -}}
{{- range .Values.worker.autoscaling.keda.excludeQueues }}{{ $excluded = append $excluded (regexReplaceAll "[^a-z0-9_]" . "") }}{{ end -}}
sum(max by (queue, state) (sourcegull_queue_jobs{state=~"queued|processing"{{ if $excluded }},queue!~"{{ join "|" $excluded }}"{{ end }}}))
{{- end -}}

{{- define "sourcegull.worker.kedaTrigger" -}}
{{- .Values.worker.autoscaling.keda.trigger -}}
{{- end -}}

{{- define "sourcegull.worker.queueDepthQuery" -}}
{{- $cols := list "state" "num_resets" "num_failures" "queued_at" "started_at" "finished_at" "process_after" "worker_hostname" "last_heartbeat_at" "failure_message" "execution_logs" "cancel" -}}
{{- $quotedCols := list -}}
{{- range $cols }}{{ $quotedCols = append $quotedCols (printf "'%s'" .) }}{{ end -}}
{{- $excluded := list -}}
{{- range .Values.worker.autoscaling.keda.excludeQueues }}{{ $excluded = append $excluded (printf "'%s'" (regexReplaceAll "[^a-z0-9_]" . "")) }}{{ end -}}
SELECT COALESCE(sum((xpath('/row/n/text()', query_to_xml(format('SELECT count(*) AS n FROM %I WHERE state = ''processing'' OR (state = ''queued'' AND (process_after IS NULL OR process_after <= now()))', q.table_name), false, true, '')))[1]::text::bigint), 0) FROM (SELECT c.table_name FROM information_schema.columns c JOIN information_schema.tables t ON t.table_schema = c.table_schema AND t.table_name = c.table_name WHERE c.table_schema = current_schema() AND t.table_type = 'BASE TABLE' AND c.column_name IN ({{ join ", " $quotedCols }})
{{- if $excluded }} AND c.table_name NOT IN ({{ join ", " $excluded }}){{ end }} GROUP BY c.table_name HAVING count(DISTINCT c.column_name) = {{ len $cols }}) q
{{- end -}}
