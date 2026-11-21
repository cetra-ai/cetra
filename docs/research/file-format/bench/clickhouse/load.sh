#!/bin/bash
# Loads one dataset into database $1 from the Parquet glob $2 (under user_files), then merges
# its parts as a long-running Langfuse would have, and prints the time and the table sizes.
set -e
C=${CLICKHOUSE:-clickhouse}
DB=$1
$C client -q "DROP DATABASE IF EXISTS $DB"
$C client -q "CREATE DATABASE $DB"
$C client --database $DB --multiquery < "$(dirname "$0")/schema.sql"
t0=$(date +%s.%N)
sed "s#{SRC}#$2#" "$(dirname "$0")/load.sql" | $C client --database $DB --multiquery
t1=$(date +%s.%N)
$C client --database $DB -q "OPTIMIZE TABLE events_full FINAL"
$C client --database $DB -q "OPTIMIZE TABLE events_core FINAL"
t2=$(date +%s.%N)
echo "load $DB insert $(echo "$t1 - $t0" | bc) s, merge $(echo "$t2 - $t1" | bc) s"
$C client -q "SELECT table, sum(rows), round(sum(bytes_on_disk)/1048576, 1) AS mib, count() AS parts FROM system.parts WHERE database = '$DB' AND active GROUP BY table ORDER BY table"
