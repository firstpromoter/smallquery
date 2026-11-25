---
paths:
  - "src/translate.rs"
  - "src/types.rs"
---

# GoogleSQL -> DuckDB translation

- Pipeline: tokenize with sqlparser's `BigQueryDialect`, re-quote backtick names
  (`requote`), parse, rewrite with the `VisitorMut` (`Rewriter`) bottom-up, print.
  Children are rewritten before their parent, so a rule sees already-translated arguments.
- Shape-changing rewrites are DuckDB SQL templates re-parsed with `DuckDbDialect`
  (`parse`). Keep new rules in that style: one arm in `function()` for a function,
  an arm in `Rewriter::rewrite` for other expressions, and `post_visit_select` /
  `post_visit_table_factor` / `post_visit_statement` for clauses and DDL.
- A function DuckDB already has under the same name and semantics needs no rule.
  Check DuckDB first (`duckdb -c "SELECT ..."`), including NULL, empty-array and
  out-of-range behaviour, before writing one.
- Types: BigQuery TIMESTAMP is DuckDB `TIMESTAMPTZ` (session in UTC), DATETIME is
  `TIMESTAMP`, NUMERIC is `DECIMAL(38,9)`. Change `duck_type` / `bq_field` together.
- `project.dataset.table` resolves natively: the database is attached under the
  project id and datasets are DuckDB schemas. Don't strip the project part.
- Query parameters become typed, escaped SQL literals (`literal`), never string
  concatenation of raw values.
- Every new rule gets a test in `translate.rs` asserting the exact DuckDB SQL, with
  generic table and column names (see the public-repository rule in `AGENTS.md`).
