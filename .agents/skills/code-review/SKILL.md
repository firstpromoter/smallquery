---
name: code-review
description: Review a pull request or diff in smallquery for BigQuery compatibility, translation correctness, REST wire format, concurrency and public-repository hygiene. Use for requested code reviews.
metadata:
  scope: smallquery
---

# smallquery code review

Read `AGENTS.md` and the `.agents/rules/` files its routing table names for the
changed paths. They own the conventions; this skill owns the review workflow.

- Compare the diff with its base (normally `main`). For translation changes, trace
  each rule through nested expressions, NULLs, empty arrays and both TIMESTAMP and
  DATETIME inputs, and check the asserted SQL actually runs on DuckDB.
- For REST changes, check field names, string encoding and error envelopes against
  the BigQuery REST reference, and that DuckDB work stays inside `blocking()`.
- **Public-repository check:** flag any internal project ids, dataset/table names,
  customers, hostnames, internal repos or tickets in code, tests, docs or commit
  messages. That is a blocking finding even when the code is otherwise fine.
- You may run `cargo test` and the server locally. Don't push, tag or publish images
  during a review-only request.
- Report defects the diff introduces, each with path:line, trigger, impact and fix.
  Keep optional cleanups separate. Finish with the checks you ran and what remains unverified.
