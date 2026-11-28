# ClickHouse with Langfuse's schema

The comparison of `real-data.md` § ClickHouse: our datasets loaded into Langfuse's
`events_full` and `events_core` tables, and the bench's queries written against them.

1. `python -I make_schema.py <langfuse>/packages/shared/clickhouse/migrations/canonical schema.sql`
2. Start a server in a data directory holding `config.xml` and `users.xml`
   (`clickhouse server --config-file=config.xml`; `query_log` gives the per-query timings),
   with the dataset directories linked under its `user_files/`.
3. `load.sh <database> '<dataset>/pq_default/*.parquet'` for each dataset: the spans of the
   `pq_default` Parquet files, mapped onto Langfuse's columns by `load.sql`, then merged.
4. `queries.sh <database> 30` (the bench's queries), or `queries.sh fineweb 5 braintrust`.
