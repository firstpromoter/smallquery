---
paths:
  - "src/main.rs"
  - "src/types.rs"
---

# REST API

- Routes mirror `https://bigquery.googleapis.com/bigquery/v2/...`. Responses carry
  the same `kind`, field names and string-encoded numbers as BigQuery's; clients
  parse them strictly. Check the REST reference for each field you add.
- Errors use BigQuery's envelope (`err()`): `{"error": {code, message, errors:
  [{reason}], status}}` with the matching HTTP code: 404 `notFound`, 409 `duplicate`,
  400 `invalidQuery`. Client libraries map exceptions from these.
- Query jobs run synchronously and are always `DONE`. A SELECT is materialised into
  `_smallquery."<jobId>"` and that table is the job's `destinationTable`, so
  `getQueryResults` and `tabledata.list` page over the same rows. A failed
  `jobs.insert` still returns 200 with `status.errorResult`, as BigQuery does;
  `jobs.query` and `getQueryResults` return the 400.
- Wire values: everything is a string; TIMESTAMP is epoch seconds with microseconds
  (`"1704103200.500000"`) or integer micros when `formatOptions.useInt64Timestamp`
  is set; repeated/record fields nest `{"v": [...]}` / `{"v": {"f": [...]}}`.
- All DuckDB work goes through `blocking()`: one connection behind a mutex, off the
  async runtime. Don't hold the lock across `.await`.
- Request bodies are parsed with `AnyJson`, whatever the Content-Type (plain
  `curl -d` sends form-encoded).
- An unimplemented endpoint falls through to the 501 fallback, which logs it. Add a
  route only together with a test or a client run that exercises it.
