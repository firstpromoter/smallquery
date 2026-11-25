# AGENTS.md — smallquery

Guidance for AI coding agents (Claude Code, Codex, Copilot, Cursor, ...) working in
this repository. It follows the [AGENTS.md](https://agents.md) convention;
`CLAUDE.md` only includes this file.

smallquery is a BigQuery emulator for local development and CI: an HTTP server
that speaks BigQuery's REST API (`/bigquery/v2/...`), translates GoogleSQL to
DuckDB SQL, and stores data in DuckDB. It aims to run real client code
unchanged: the official Python and Ruby clients, dbt-bigquery, and plain REST.

## This is a public repository

Everything here (code, comments, tests, fixtures, docs, commit messages, issues,
PR descriptions) is public. **Never mention anything about the internal systems
of the people or companies using smallquery**: no company-internal project ids,
dataset or table names, customer names, hostnames, internal repositories,
tickets, incidents, Slack channels, credentials or business logic. That holds
even when the work was prompted by an internal bug or the request names one.

- Use generic examples: `my-project`, `my_dataset`, `events`, `orders`,
  `user_id`. Reduce an internal query to the smallest generic SQL that shows the
  same behaviour before it goes into a test or a commit.
- Describe motivation in terms of BigQuery behaviour ("dbt-bigquery reads
  `destinationTable` after a CTAS"), never "our reports needed this".
- If you cannot make a change without internal context, stop and ask instead of
  committing it.

## Layout

```
src/main.rs        HTTP server (axum): routes, jobs, datasets, tables, insertAll, paging
src/translate.rs   GoogleSQL -> DuckDB: sqlparser AST rewrites + function table
src/types.rs       BigQuery <-> DuckDB types and the REST row/value wire format
Dockerfile         two-stage build; dependencies cached in their own layer
.github/workflows/image.yml   test, then multi-arch image to ghcr.io (native amd64 + arm64 runners)
```

## Commands

```bash
cargo test                       # unit tests (translator + wire format)
cargo run -- --project=my-project --dataset=my_dataset --port=9050
curl -s localhost:9050/bigquery/v2/projects/my-project/queries \
  -d '{"query": "SELECT 1", "useLegacySql": false}'
docker build -t smallquery:dev .  # slow the first time: bundled DuckDB compiles from C++
```

A failed query logs the original SQL to stderr, along with DuckDB's error on the translated SQL.

## Working here

- Before calling a change done, run `cargo test`. For anything a client would see,
  also start the server and exercise it the way that client does (curl for REST,
  the Python client, dbt-bigquery or the Ruby gem), and say which ones you ran.
- Match BigQuery, not goccy/bigquery-emulator. The reference is the BigQuery docs:
  [GoogleSQL functions](https://cloud.google.com/bigquery/docs/reference/standard-sql/functions-and-operators)
  and the [REST reference](https://cloud.google.com/bigquery/docs/reference/rest).
  Where the two differ, link the BigQuery doc in the comment.
- Keep it small. Prefer one rewrite rule over a new module, and DuckDB built-ins over
  Rust code. A known divergence gets a one-line `shortcut: <limit>, <when to fix>`
  comment and a line under "Known gaps" in `README.md`.
- New dependencies need a reason in the PR; the image should stay one static-ish binary.
- Open PRs as drafts (`gh pr create --draft`) against `main`; mark ready only when asked.
  Don't push to `main` or force-push unless explicitly asked. Pushes to `main` and
  `v*` tags publish public images.

## Rules and skills

Load the rule for the area you touch:

| Change | Rule |
|---|---|
| GoogleSQL translation, functions, types in SQL | [translation](.agents/rules/translation.md) |
| REST endpoints, jobs, paging, value encoding | [rest-api](.agents/rules/rest-api.md) |
| Dockerfile, workflows, releases, image tags | [release](.agents/rules/release.md) |

Skills: [support-googlesql](.agents/skills/support-googlesql/SKILL.md) for making a
failing query work, [code-review](.agents/skills/code-review/SKILL.md) for reviews.
They live at `.agents/skills/<name>/SKILL.md`; if your client does not discover
them, read the file directly.
