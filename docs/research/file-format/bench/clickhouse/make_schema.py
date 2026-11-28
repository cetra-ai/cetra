"""Writes Langfuse's ClickHouse schema for the comparison (`real-data.md` § ClickHouse).

Reads the migrations that create `events_full`, `events_core` and its materialized view, and
their later indexes, from a Langfuse checkout, and removes the cluster placeholders. The
schema is not copied into this repository.

    python -I make_schema.py <langfuse>/packages/shared/clickhouse/migrations/canonical schema.sql
"""

import re
import sys

MIGRATIONS = [
    "0039_create_events_full.up.sql",
    "0040_create_events_core.up.sql",
    "0041_create_events_core_mv.up.sql",
    "0043_add_events_metadata_ngram_index.up.sql",
    "0049_add_events_name_ngram_indexes.up.sql",
    "0056_compute_event_bytes.up.sql",
]


def main() -> None:
    src, out = sys.argv[1], sys.argv[2]
    parts = []
    for name in MIGRATIONS:
        with open(f"{src}/{name}") as f:
            sql = f.read()
        sql = sql.replace("{CLICKHOUSE_CLUSTER_CLAUSE}", "")
        sql = sql.replace("{CLICKHOUSE_REPLICATION_PREFIX}", "")
        sql = re.sub(r"\{CLICKHOUSE_CLUSTERED_ONLY:[^}]*\}", "", sql)
        parts.append(f"-- {name}\n" + sql.strip().rstrip(";") + ";")
    with open(out, "w") as f:
        f.write("\n\n".join(parts) + "\n")


if __name__ == "__main__":
    main()
