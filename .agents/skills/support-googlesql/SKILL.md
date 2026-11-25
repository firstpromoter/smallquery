---
name: support-googlesql
description: Make a GoogleSQL query, function or client call that fails or returns wrong results on smallquery behave like BigQuery. Use when a user reports a query error, a "smallquery does not implement" 501, or a value that differs from BigQuery.
metadata:
  scope: smallquery
---

# Support a GoogleSQL feature

Read `AGENTS.md` (especially the public-repository rule) and the rule for the
area: `.agents/rules/translation.md` for SQL, `.agents/rules/rest-api.md` for endpoints.

1. **Reproduce generically.** Reduce the failing query to the smallest SQL over
   generic names (`my_dataset.events`) that still fails. Internal names, data or
   context never go into the repository.
2. **Look up BigQuery's behaviour** in the GoogleSQL docs: argument order, NULL
   handling, return type, edge cases (empty arrays, out-of-range, time zones).
3. **Check DuckDB.** Often a built-in matches already and only the name or argument
   order differs. Test it with `duckdb -c`.
4. **Add the smallest rule**: a `function()` arm, an expression or clause rewrite,
   or a route. Add a unit test asserting the translated SQL.
5. **Verify end to end**: `cargo test`, then start the server and run the reduced
   query through `/queries` (and through the reporting client if it was a client
   issue). Compare the result with BigQuery's documented output.
6. If exact parity is out of reach, ship the closest behaviour with a
   `shortcut:` comment and a "Known gaps" line in `README.md`.

Report what you changed, what you ran, and any remaining divergence from BigQuery.
