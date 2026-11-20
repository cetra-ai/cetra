#!/bin/bash
# The bench's queries on Langfuse's tables in database $1 (a dataset), each run $2 times on
# one connection; prints the median server-side time (query_log, microseconds), the median
# CPU time, and the result. The query cache and query condition cache are off: they return
# results, or skip granules, from earlier runs of the same query.
C=${CLICKHOUSE:-clickhouse}
DB=$1; N=${2:-10}; RUN=$(date +%s%N)
T0=1772409600000000   # datagen::DAY_START in microseconds
H=3600000000
ts() { echo "fromUnixTimestamp64Micro(toInt64($1))"; }
S="--use_query_cache=0 --use_query_condition_cache=0"
run() {
  local name=$1 sql=$2 tag="$RUN-$1"
  for i in $(seq 1 $N); do echo "$sql FORMAT Null;"; done | $C client --database $DB $S --log_comment="$tag" --multiquery
  local res
  case "$sql" in
    "SELECT count() FROM"*) res="count $($C client --database $DB $S -q "$sql FORMAT TSV" 2>&1 | head -c 100 | tr '\n\t' '; ')" ;;
    *) res="rows $($C client --database $DB $S -q "SELECT count() FROM ($sql) FORMAT TSV" 2>&1 | head -c 100)" ;;
  esac
  $C client -q "SYSTEM FLUSH LOGS query_log"
  local stats=$($C client -q "SELECT round(quantileExact(0.5)(dateDiff('microsecond', query_start_time_microseconds, event_time_microseconds)) / 1000, 2), round(quantileExact(0.5)(ProfileEvents['UserTimeMicroseconds'] + ProfileEvents['SystemTimeMicroseconds']) / 1000, 1), round(quantileExact(0.5)(read_bytes) / 1024, 1) FROM system.query_log WHERE log_comment = '$tag' AND type = 'QueryFinish' FORMAT TSV")
  printf "ch %-24s %s  %s\n" "$name" "$(echo "$stats" | awk '{printf "median %9s ms  cpu %8s ms  read KiB %10s", $1, $2, $3}')" "$res"
}
if [ "$3" != braintrust ]; then
run count_errors_6h "SELECT count() FROM events_core WHERE level = 'ERROR' AND start_time >= $(ts "$T0 + 10 * $H") AND start_time < $(ts "$T0 + 16 * $H")"
run model_rollup_day "SELECT provided_model_name, count(), sum(usage_details['total']), sum(cost_details['total']), avg(dateDiff('nanosecond', start_time, end_time)) FROM events_core WHERE type = 'GENERATION' GROUP BY provided_model_name ORDER BY provided_model_name"
run list_llm_1h "SELECT trace_id, span_id, name, start_time, dateDiff('nanosecond', start_time, end_time), level, provided_model_name, usage_details['total'], cost_details['total'], input FROM events_core WHERE type = 'GENERATION' AND start_time >= $(ts "$T0 + 14 * $H") AND start_time < $(ts "$T0 + 15 * $H") ORDER BY start_time DESC, trace_id DESC, span_id DESC LIMIT 50"
run user_filter "SELECT count(), sum(cost_details['total']) FROM events_core WHERE user_id = 'user-1234'"
run metadata_filter "SELECT count() FROM events_core WHERE metadata_values[indexOf(metadata_names, 'customer_id')] = '\"cust-42\"'"
run substring_lf "SELECT count() FROM events_full WHERE position(lower(input), 'zqxjneedle') > 0 AND hasAllTokens(lower(input), ['zqxjneedle'])"
run substring_exact "SELECT count() FROM events_full WHERE position(input, 'zqxjneedle') > 0"
run scan_all_payload "SELECT sum(lengthUTF8(input)) + sum(lengthUTF8(output)) FROM events_full"
run scan_lengths_col "SELECT sum(input_length) + sum(output_length) FROM events_full"
# Trace lookups: the dataset's first 20 trace ids, each run N times; the median over all runs.
for kind in full light; do
  tag="$RUN-trace_lookup_$kind"
  for id in $(head -20 ${BENCH:-~/bench4}/$DB/trace_ids.txt); do
    if [ $kind = full ]; then sql="SELECT * FROM events_full WHERE trace_id = '$id'";
    else sql="SELECT span_id, parent_span_id, name, start_time, end_time, level, provided_model_name, usage_details['total'], cost_details['total'] FROM events_core WHERE trace_id = '$id'"; fi
    for i in $(seq 1 3); do echo "$sql FORMAT Null;"; done
  done | $C client --database $DB $S --log_comment="$tag" --multiquery
  $C client -q "SYSTEM FLUSH LOGS query_log"
  stats=$($C client -q "SELECT round(quantileExact(0.5)(dateDiff('microsecond', query_start_time_microseconds, event_time_microseconds)) / 1000, 2), round(quantileExact(0.5)(ProfileEvents['UserTimeMicroseconds'] + ProfileEvents['SystemTimeMicroseconds']) / 1000, 1), round(avg(read_bytes) / 1024, 1), sum(result_rows) FROM system.query_log WHERE log_comment = '$tag' AND type = 'QueryFinish' FORMAT TSV")
  printf "ch %-24s %s\n" "trace_lookup_$kind" "$(echo "$stats" | awk '{printf "median %9s ms  cpu %8s ms  read KiB %10s  rows %s (20 ids x 3)", $1, $2, $3, $4}')"
done
fi
if [ "$3" = braintrust ]; then
END=$(( T0 + 3 * 24 * H ))
NONE="under the red bridge the yellow car slowly went home"
RARE="red car under the yellow bridge"
SINGLE="when the red car slowly went under the yellow bridge"
# Langfuse's input search (events-predicate.ts): case-insensitive position, with the text
# index prefilter on the phrase's tokens. "exact" is the case-sensitive byte search Strata,
# Parquet and Vortex run, which no index of the table serves.
lf() { echo "position(lower(input), lower('$1')) > 0 AND hasAllTokens(lower(input), arraySlice(arrayDistinct(tokens(lower('$1'))), 1, 64))"; }
ex() { echo "position(input, '$1') > 0"; }
for form in lf ex; do
  run "bt_phrase_none_$form" "SELECT count() FROM events_full WHERE $($form "$NONE")"
  run "bt_help_me_$form" "SELECT count() FROM events_full WHERE $($form "help me")"
  run "bt_newest_help_me_$form" "SELECT input FROM events_full WHERE $($form "help me") ORDER BY start_time DESC LIMIT 100"
  run "bt_newest_rare_$form" "SELECT input FROM events_full WHERE $($form "$RARE") ORDER BY start_time DESC LIMIT 100"
  run "bt_single_$form" "SELECT input FROM events_full WHERE $($form "$SINGLE") ORDER BY start_time DESC LIMIT 100"
  run "bt_help_me_1d_$form" "SELECT count() FROM events_full WHERE $($form "help me") AND start_time >= $(ts "$END - 24 * $H")"
  run "bt_help_me_1h_$form" "SELECT count() FROM events_full WHERE $($form "help me") AND start_time >= $(ts "$END - $H")"
  run "bt_newest_help_me_5m_$form" "SELECT input FROM events_full WHERE $($form "help me") AND start_time >= $(ts "$END - 300000000") ORDER BY start_time DESC LIMIT 100"
  run "bt_newest_rare_1h_$form" "SELECT input FROM events_full WHERE $($form "$RARE") AND start_time >= $(ts "$END - $H") ORDER BY start_time DESC LIMIT 100"
done
run bt_spans_per_hour "SELECT toStartOfHour(start_time) AS h, count() FROM events_core GROUP BY h ORDER BY h"
fi
