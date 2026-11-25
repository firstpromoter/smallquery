# smallquery

A small BigQuery emulator for local development and CI, written in Rust on
[DuckDB](https://duckdb.org). It speaks BigQuery's REST API, translates GoogleSQL
to DuckDB SQL, and runs dbt-bigquery projects plus the official Python and Ruby
clients without code changes.

It is a drop-in for [goccy/bigquery-emulator](https://github.com/goccy/bigquery-emulator)
in the common cases: same flags, same port, same `BIGQUERY_EMULATOR_HOST` setup.

## Run

```bash
docker run --rm -p 9050:9050 -v smallquery:/data ghcr.io/firstpromoter/smallquery \
  --project=my-project --dataset=my_dataset --database=/data/db.duckdb --port=9050
```

```yaml
# docker-compose.yml
bigquery-emulator:
  image: ghcr.io/firstpromoter/smallquery:latest
  command: [--project=my-project, --dataset=my_dataset, --database=/data/db.duckdb, --port=9050]
  ports: ["9050:9050"]
  volumes: [bigquery-data:/data]
```

Then point clients at it with `BIGQUERY_EMULATOR_HOST=http://localhost:9050`.
No credentials are checked.

| Flag | Default | |
|---|---|---|
| `--project` | `test` | Project id; `project.dataset.table` names resolve against it |
| `--dataset` | none | Dataset to create at startup (repeatable) |
| `--database` | in-memory | DuckDB file to persist data in |
| `--port` | `9050` | HTTP port |

Unknown flags (goccy's `--grpc-port`, `--log-level`) are ignored.

```bash
curl -s localhost:9050/bigquery/v2/projects/my-project/queries \
  -d '{"query": "SELECT 1 + 1", "useLegacySql": false}'
```

## What works

**REST API:** datasets (insert/get/list/delete), tables (insert incl. views,
get/list/delete), `tabledata.list` and `insertAll`, query jobs (`jobs.insert`,
`jobs.get`, `jobs.getQueryResults`, `jobs.query`) with paging, named and
positional query parameters, and `formatOptions.useInt64Timestamp`. Jobs report
their real `statementType` and `destinationTable`, so dbt-bigquery needs no patches.

**GoogleSQL**, rewritten on the parsed AST: backtick names, `SAFE_CAST`, the
`INT64`/`STRING`/`NUMERIC`/`ARRAY<>`/`STRUCT<>` types, `DATE`/`DATETIME`/`TIMESTAMP`
arithmetic (`DATE_SUB(d, INTERVAL 1 DAY)`, `DATE_DIFF`, `DATE_TRUNC(d, WEEK(MONDAY))`),
`arr[OFFSET(n)]`, `IN UNNEST(...)`, `UNNEST ... WITH OFFSET`, `STRUCT(...)`,
`* EXCEPT`, `ARRAY_AGG(... ORDER BY ... LIMIT n)`, `COUNTIF`, `SAFE_DIVIDE`, `TRUNC(x, n)`,
JSON functions, `PARSE_*`/`FORMAT_*`, `dataset.INFORMATION_SCHEMA.COLUMNS/TABLES`,
`f0_` column naming, and DDL with `PARTITION BY`/`CLUSTER BY`/`OPTIONS` (accepted and ignored).
DuckDB already supports `QUALIFY`, `MERGE`, window functions and most of the rest.

TIMESTAMP is stored as DuckDB `TIMESTAMPTZ` (session in UTC), DATETIME as `TIMESTAMP`.

## Known gaps

- Load jobs, extract jobs, the Storage Read/Write gRPC APIs, routines (UDFs) and scripting (`DECLARE`/`SET`)
  are not implemented: the server answers `501` and logs the request.
- `DATE_DIFF(..., WEEK)` uses ISO (Monday) weeks; BigQuery uses Sunday weeks.
- `insertAll` stores a wrongly typed value as NULL instead of returning `insertErrors`.
- Timestamps nested inside arrays or structs come back as text, not epoch seconds.
- Table `PATCH`/`PUT` (descriptions, labels, schema changes) are accepted and do nothing.
- Only the most recent 500 query results are kept for paging.

Every rewrite lives in `src/translate.rs`. A missing function is usually one line in `function()`.

## Develop

```bash
cargo test
cargo run -- --project=my-project --dataset=my_dataset
```

A failed query logs the original SQL to stderr, along with DuckDB's error on the translated SQL.

Images are built by `.github/workflows/image.yml` for `linux/amd64` and `linux/arm64`:
`:latest` and `:sha-<short>` from `main`, `:X.Y.Z` from `vX.Y.Z` tags.

## License

MIT, see [LICENSE](LICENSE).
