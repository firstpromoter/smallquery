//! smallquery: a local BigQuery REST emulator on DuckDB.
//!
//! Covers what the Python (incl. dbt-bigquery) and Ruby clients call: datasets, tables, tabledata.insertAll /
//! list, and query jobs (jobs.insert / get / getQueryResults, jobs.query). Every
//! SELECT result is materialised as a table in the hidden `_smallquery` schema, so
//! getQueryResults and tabledata.list on the job's destination read the same rows.
//!
//! Drop-in for goccy/bigquery-emulator's flags: --project, --dataset, --database, --port.

mod translate;
mod types;

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use duckdb::Connection;
use serde_json::{Value, json};

use types::{bq_field, cell, duck_type_of_field, quote, wire_expr};

const ANON: &str = "_smallquery";
const HIDDEN: &[&str] = &["main", ANON, "information_schema", "pg_catalog"];
/// shortcut: keeps the last N query results; raise it if a client pages through older jobs.
const KEEP_JOBS: usize = 500;

type Resp = (StatusCode, Json<Value>);
type Res<T> = Result<T, Resp>;

struct Job {
    id: String,
    location: String,
    sql: String,
    statement_type: &'static str,
    dest: Option<(String, String)>,
    dml_rows: Option<usize>,
    error: Option<String>,
    ms: u128,
}

struct Inner {
    db: Connection,
    project: String,
    jobs: HashMap<String, Job>,
    order: VecDeque<String>,
}

type Shared = Arc<Mutex<Inner>>;

fn main() {
    let mut project = "test".to_string();
    let (mut database, mut port, mut datasets) = (None, 9050u16, vec![]);
    let args: Vec<String> = std::env::args().skip(1).collect();
    for a in &args {
        // Unknown goccy flags (--grpc-port, --log-level) are accepted and ignored.
        let (k, v) = a.split_once('=').unwrap_or((a, ""));
        match k {
            "--project" => project = v.into(),
            "--database" => database = Some(v.to_string()),
            "--port" => port = v.parse().expect("--port"),
            "--dataset" => datasets.push(v.to_string()),
            _ => {}
        }
    }

    let db = Connection::open_in_memory().expect("duckdb");
    let file = database.as_deref().unwrap_or(":memory:").replace('\'', "''");
    db.execute_batch(&format!(
        "SET TimeZone = 'UTC';
         ATTACH '{file}' AS {p}; USE {p};
         DROP SCHEMA IF EXISTS {ANON} CASCADE; CREATE SCHEMA {ANON};
         CREATE OR REPLACE TEMP MACRO bq_scalar(t) AS CASE
           WHEN t IN ('TINYINT','SMALLINT','INTEGER','BIGINT','HUGEINT','UTINYINT','USMALLINT','UINTEGER','UBIGINT') THEN 'INT64'
           WHEN t IN ('FLOAT','DOUBLE') THEN 'FLOAT64'
           WHEN t LIKE 'DECIMAL%' THEN 'NUMERIC'
           WHEN t = 'VARCHAR' THEN 'STRING'
           WHEN t = 'BOOLEAN' THEN 'BOOL'
           WHEN t = 'TIMESTAMP WITH TIME ZONE' THEN 'TIMESTAMP'
           WHEN t = 'TIMESTAMP' THEN 'DATETIME'
           WHEN t = 'BLOB' THEN 'BYTES'
           ELSE t END;
         CREATE OR REPLACE TEMP MACRO bq_type(t) AS CASE
           WHEN t LIKE '%[]' THEN 'ARRAY<' || bq_scalar(left(t, length(t) - 2)) || '>' ELSE bq_scalar(t) END;",
        p = quote(&project)
    ))
    .expect("attach database");
    for d in &datasets {
        db.execute_batch(&format!("CREATE SCHEMA IF NOT EXISTS {}", quote(d))).expect("dataset");
    }

    let state: Shared = Arc::new(Mutex::new(Inner { db, project, jobs: HashMap::new(), order: VecDeque::new() }));
    let p = "/bigquery/v2/projects/{project}";
    let app = Router::new()
        .route(&format!("{p}/queries"), post(jobs_query))
        .route(&format!("{p}/queries/{{job}}"), get(query_results))
        .route(&format!("{p}/jobs"), post(jobs_insert))
        .route(&format!("{p}/jobs/{{job}}"), get(jobs_get))
        .route(&format!("{p}/jobs/{{job}}/cancel"), post(jobs_get))
        .route(&format!("{p}/datasets"), get(datasets_list).post(datasets_insert))
        .route(&format!("{p}/datasets/{{d}}"), get(datasets_get).delete(datasets_delete).patch(datasets_get).put(datasets_get))
        .route(&format!("{p}/datasets/{{d}}/tables"), get(tables_list).post(tables_insert))
        .route(&format!("{p}/datasets/{{d}}/tables/{{t}}"), get(tables_get).delete(tables_delete).patch(tables_get).put(tables_get))
        .route(&format!("{p}/datasets/{{d}}/tables/{{t}}/data"), get(tabledata_list))
        .route(&format!("{p}/datasets/{{d}}/tables/{{t}}/insertAll"), post(insert_all))
        // No UDF support: every routine is absent, which is what dbt checks for.
        .route(&format!("{p}/datasets/{{d}}/routines/{{r}}"), get(|| async { not_found("Routine") }))
        .fallback(|req: axum::extract::Request| async move {
            eprintln!("smallquery: unsupported {} {}", req.method(), req.uri());
            err(StatusCode::NOT_IMPLEMENTED, "notImplemented", &format!("smallquery does not implement {} {}", req.method(), req.uri().path()))
        })
        .with_state(state);

    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await.expect("bind");
        eprintln!("smallquery listening on :{port}");
        axum::serve(listener, app).await.unwrap();
    });
}

// ---- plumbing ---------------------------------------------------------------

fn err(code: StatusCode, reason: &str, message: &str) -> Resp {
    let status = match code.as_u16() {
        404 => "NOT_FOUND",
        409 => "ALREADY_EXISTS",
        501 => "UNIMPLEMENTED",
        _ => "INVALID_ARGUMENT",
    };
    let e = json!({"message": message, "domain": "global", "reason": reason});
    (code, Json(json!({"error": {"code": code.as_u16(), "message": message, "errors": [e], "status": status}})))
}

fn invalid(m: impl ToString) -> Resp {
    err(StatusCode::BAD_REQUEST, "invalidQuery", &m.to_string())
}

fn not_found(what: &str) -> Resp {
    err(StatusCode::NOT_FOUND, "notFound", &format!("Not found: {what}"))
}

fn ok(v: Value) -> Res<Resp> {
    Ok((StatusCode::OK, Json(v)))
}

/// Run blocking DuckDB work off the async runtime, under the one connection's lock.
async fn blocking(st: Shared, f: impl FnOnce(&mut Inner) -> Res<Resp> + Send + 'static) -> Resp {
    tokio::task::spawn_blocking(move || {
        let mut inner = st.lock().unwrap_or_else(|p| p.into_inner());
        f(&mut inner).unwrap_or_else(|e| e)
    })
    .await
    .unwrap_or_else(|e| err(StatusCode::INTERNAL_SERVER_ERROR, "internalError", &e.to_string()))
}

/// A JSON body whatever its Content-Type: `curl -d` (as in the docs) sends form-encoded.
struct AnyJson(Value);

impl<S: Send + Sync> axum::extract::FromRequest<S> for AnyJson {
    type Rejection = Resp;

    async fn from_request(req: axum::extract::Request, st: &S) -> Result<Self, Resp> {
        let bytes = axum::body::Bytes::from_request(req, st).await.map_err(invalid)?;
        serde_json::from_slice(&bytes).map(AnyJson).map_err(|e| invalid(format!("request body is not JSON: {e}")))
    }
}

fn now_ms() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis()
}

fn table_ref(project: &str, d: &str, t: &str) -> Value {
    json!({"projectId": project, "datasetId": d, "tableId": t})
}

/// Query-string numbers arrive as strings, JSON-body numbers as either.
fn num(v: Option<&Value>) -> Option<usize> {
    match v? {
        Value::Number(n) => n.as_u64().map(|n| n as usize),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

// ---- catalog ----------------------------------------------------------------

impl Inner {
    fn dataset_exists(&self, d: &str) -> bool {
        self.db
            .query_row(
                "SELECT count(*) FROM information_schema.schemata WHERE catalog_name = current_database() AND schema_name = ?",
                [d],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0)
            > 0
    }

    /// (table_type, [(column, duckdb type)]), or None if absent.
    fn table(&self, d: &str, t: &str) -> Option<(String, Vec<(String, String)>)> {
        let kind: String = self
            .db
            .query_row(
                "SELECT table_type FROM information_schema.tables WHERE table_catalog = current_database() AND table_schema = ? AND table_name = ?",
                [d, t],
                |r| r.get(0),
            )
            .ok()?;
        let mut stmt = self
            .db
            .prepare(
                "SELECT column_name, data_type FROM information_schema.columns
                 WHERE table_catalog = current_database() AND table_schema = ? AND table_name = ? ORDER BY ordinal_position",
            )
            .ok()?;
        let cols = stmt.query_map([d, t], |r| Ok((r.get(0)?, r.get(1)?))).ok()?.filter_map(Result::ok).collect();
        Some((kind, cols))
    }

    fn table_json(&self, d: &str, t: &str) -> Res<Value> {
        let (kind, cols) = self.table(d, t).ok_or_else(|| not_found(&format!("Table {}:{d}.{t}", self.project)))?;
        let fields: Vec<Value> = cols.iter().map(|(n, ty)| bq_field(n, ty)).collect();
        let mut v = json!({
            "kind": "bigquery#table",
            "id": format!("{}:{d}.{t}", self.project),
            "tableReference": table_ref(&self.project, d, t),
            "schema": {"fields": fields},
            "type": if kind == "VIEW" { "VIEW" } else { "TABLE" },
            "numBytes": "0",
            "location": "US",
            "creationTime": now_ms().to_string(),
            "lastModifiedTime": now_ms().to_string(),
        });
        if kind != "VIEW" {
            let n: i64 = self
                .db
                .query_row(&format!("SELECT count(*) FROM {}.{}", quote(d), quote(t)), [], |r| r.get(0))
                .map_err(invalid)?;
            v["numRows"] = json!(n.to_string());
        }
        Ok(v)
    }

    /// Rows [start, start+max) of a table in BigQuery's `{f: [{v}]}` form.
    fn rows(&self, d: &str, t: &str, start: usize, max: Option<usize>, int64_ts: bool) -> Res<(Value, Vec<Value>, usize)> {
        let (_, cols) = self.table(d, t).ok_or_else(|| not_found(&format!("Table {}:{d}.{t}", self.project)))?;
        let fields: Vec<Value> = cols.iter().map(|(n, ty)| bq_field(n, ty)).collect();
        let from = format!("{}.{}", quote(d), quote(t));
        let total: i64 = self.db.query_row(&format!("SELECT count(*) FROM {from}"), [], |r| r.get(0)).map_err(invalid)?;
        let mut rows = vec![];
        if !cols.is_empty() && max != Some(0) {
            let select: Vec<String> = cols.iter().map(|(n, ty)| wire_expr(n, ty)).collect();
            let limit = max.map(|m| format!(" LIMIT {m}")).unwrap_or_default();
            let sql = format!("SELECT {} FROM {from}{limit} OFFSET {start}", select.join(", "));
            let mut stmt = self.db.prepare(&sql).map_err(invalid)?;
            let mut q = stmt.query([]).map_err(invalid)?;
            while let Some(r) = q.next().map_err(invalid)? {
                let cells: Vec<Value> = fields
                    .iter()
                    .enumerate()
                    .map(|(i, f)| cell(f, r.get::<_, Option<String>>(i).ok().flatten(), int64_ts))
                    .collect();
                rows.push(json!({"f": cells}));
            }
        }
        Ok((json!({"fields": fields}), rows, total as usize))
    }

    fn run(&mut self, id: String, location: String, sql: String, params: &[Value]) {
        let ms = now_ms();
        let mut job = Job { id: id.clone(), location, sql, statement_type: "SELECT", dest: None, dml_rows: None, error: None, ms };
        if let Err(e) = self.execute(&mut job, params) {
            eprintln!("smallquery: query failed: {e}\n--- sql ---\n{}\n-----------", job.sql);
            job.error = Some(e);
        }
        self.order.push_back(id.clone());
        while self.order.len() > KEEP_JOBS {
            if let Some(old) = self.order.pop_front() {
                let _ = self.db.execute_batch(&format!("DROP TABLE IF EXISTS {ANON}.{}", quote(&old)));
                self.jobs.remove(&old);
            }
        }
        self.jobs.insert(id, job);
    }

    fn execute(&self, job: &mut Job, params: &[Value]) -> Result<(), String> {
        for s in translate::translate(&job.sql, params)? {
            job.statement_type = s.statement_type;
            job.dest = s.target;
            match s.statement_type {
                "SELECT" => {
                    let sql = format!("CREATE OR REPLACE TABLE {ANON}.{} AS {}", quote(&job.id), s.sql);
                    self.db.execute_batch(&sql).map_err(|e| e.to_string())?;
                    job.dest = Some((ANON.into(), job.id.clone()));
                }
                "INSERT" | "UPDATE" | "DELETE" | "MERGE" => {
                    job.dml_rows = Some(self.db.execute(&s.sql, []).map_err(|e| e.to_string())?);
                }
                _ => self.db.execute_batch(&s.sql).map_err(|e| e.to_string())?,
            }
        }
        Ok(())
    }

    fn job_json(&self, j: &Job) -> Value {
        let mut q = json!({"query": j.sql, "useLegacySql": false});
        if let Some((d, t)) = &j.dest {
            q["destinationTable"] = table_ref(&self.project, d, t);
        }
        let mut status = json!({"state": "DONE"});
        if let Some(e) = &j.error {
            let er = json!({"reason": "invalidQuery", "location": "query", "message": e});
            status["errorResult"] = er.clone();
            status["errors"] = json!([er]);
        }
        let mut query_stats = json!({"statementType": j.statement_type, "totalBytesProcessed": "0", "cacheHit": false});
        if let Some(n) = j.dml_rows {
            query_stats["numDmlAffectedRows"] = json!(n.to_string());
        }
        let ms = j.ms.to_string();
        json!({
            "kind": "bigquery#job",
            "id": format!("{}:{}.{}", self.project, j.location, j.id),
            "jobReference": self.job_ref(j),
            "configuration": {"jobType": "QUERY", "query": q},
            "status": status,
            "statistics": {"creationTime": ms, "startTime": ms, "endTime": ms, "totalBytesProcessed": "0", "query": query_stats},
            "user_email": "smallquery@localhost",
        })
    }

    fn job_ref(&self, j: &Job) -> Value {
        json!({"projectId": self.project, "jobId": j.id, "location": j.location})
    }

    /// The getQueryResults / jobs.query body for a finished job.
    fn results(&self, j: &Job, start: usize, max: Option<usize>, int64_ts: bool) -> Res<Value> {
        if let Some(e) = &j.error {
            return Err(invalid(e));
        }
        let mut v = json!({
            "kind": "bigquery#getQueryResultsResponse",
            "jobReference": self.job_ref(j),
            "jobComplete": true,
            "cacheHit": false,
            "totalBytesProcessed": "0",
            "schema": {"fields": []},
            "totalRows": "0",
        });
        if let Some(n) = j.dml_rows {
            v["numDmlAffectedRows"] = json!(n.to_string());
        }
        if j.statement_type == "SELECT" {
            let (d, t) = j.dest.as_ref().unwrap();
            let (schema, rows, total) = self.rows(d, t, start, max, int64_ts)?;
            let end = start + rows.len();
            v["schema"] = schema;
            v["rows"] = json!(rows);
            v["totalRows"] = json!(total.to_string());
            if end < total && max != Some(0) {
                v["pageToken"] = json!(end.to_string());
            }
        }
        Ok(v)
    }

    fn job(&self, id: &str) -> Res<&Job> {
        self.jobs.get(id).ok_or_else(|| not_found(&format!("Job {}:{id}", self.project)))
    }
}

fn page(q: &HashMap<String, String>) -> (usize, Option<usize>, bool) {
    let start = q.get("pageToken").or(q.get("startIndex")).and_then(|s| s.parse().ok()).unwrap_or(0);
    let max = q.get("maxResults").and_then(|s| s.parse().ok());
    let int64_ts = q.get("formatOptions.useInt64Timestamp").is_some_and(|v| v == "true");
    (start, max, int64_ts)
}

// ---- jobs -------------------------------------------------------------------

async fn jobs_insert(State(st): State<Shared>, AnyJson(body): AnyJson) -> Resp {
    blocking(st, move |s| {
        let cfg = &body["configuration"]["query"];
        let Some(sql) = cfg["query"].as_str() else {
            return Err(err(StatusCode::NOT_IMPLEMENTED, "notImplemented", "smallquery only runs query jobs"));
        };
        if cfg["useLegacySql"] == json!(true) {
            return Err(invalid("smallquery does not support legacy SQL"));
        }
        let id = body["jobReference"]["jobId"].as_str().map(String::from).unwrap_or_else(new_job_id);
        let loc = body["jobReference"]["location"].as_str().unwrap_or("US").to_string();
        let params = cfg["queryParameters"].as_array().cloned().unwrap_or_default();
        s.run(id.clone(), loc, sql.to_string(), &params);
        ok(s.job_json(s.job(&id)?))
    })
    .await
}

async fn jobs_query(State(st): State<Shared>, AnyJson(body): AnyJson) -> Resp {
    blocking(st, move |s| {
        let sql = body["query"].as_str().ok_or_else(|| invalid("missing query"))?.to_string();
        let loc = body["location"].as_str().unwrap_or("US").to_string();
        let params = body["queryParameters"].as_array().cloned().unwrap_or_default();
        let max = num(body.get("maxResults"));
        let int64_ts = body["formatOptions"]["useInt64Timestamp"] == json!(true);
        let id = new_job_id();
        s.run(id.clone(), loc, sql, &params);
        let mut v = s.results(s.job(&id)?, 0, max, int64_ts)?;
        v["kind"] = json!("bigquery#queryResponse");
        ok(v)
    })
    .await
}

async fn jobs_get(State(st): State<Shared>, Path((_, job)): Path<(String, String)>) -> Resp {
    blocking(st, move |s| ok(s.job_json(s.job(&job)?))).await
}

async fn query_results(
    State(st): State<Shared>,
    Path((_, job)): Path<(String, String)>,
    Query(q): Query<HashMap<String, String>>,
) -> Resp {
    blocking(st, move |s| {
        let (start, max, int64_ts) = page(&q);
        ok(s.results(s.job(&job)?, start, max, int64_ts)?)
    })
    .await
}

fn new_job_id() -> String {
    format!("job_{}", uuid::Uuid::new_v4().simple())
}

// ---- datasets ---------------------------------------------------------------

fn dataset_json(project: &str, d: &str) -> Value {
    json!({
        "kind": "bigquery#dataset",
        "id": format!("{project}:{d}"),
        "datasetReference": {"projectId": project, "datasetId": d},
        "location": "US",
    })
}

async fn datasets_list(State(st): State<Shared>) -> Resp {
    blocking(st, |s| {
        let mut stmt = s
            .db
            .prepare("SELECT schema_name FROM information_schema.schemata WHERE catalog_name = current_database() ORDER BY 1")
            .map_err(invalid)?;
        let names: Vec<String> = stmt.query_map([], |r| r.get(0)).map_err(invalid)?.filter_map(Result::ok).collect();
        let list: Vec<Value> =
            names.iter().filter(|n| !HIDDEN.contains(&n.as_str())).map(|n| dataset_json(&s.project, n)).collect();
        ok(json!({"kind": "bigquery#datasetList", "datasets": list}))
    })
    .await
}

async fn datasets_insert(State(st): State<Shared>, AnyJson(body): AnyJson) -> Resp {
    blocking(st, move |s| {
        let d = body["datasetReference"]["datasetId"].as_str().ok_or_else(|| invalid("missing datasetId"))?;
        if s.dataset_exists(d) {
            return Err(err(StatusCode::CONFLICT, "duplicate", &format!("Already Exists: Dataset {}:{d} (already exists)", s.project)));
        }
        s.db.execute_batch(&format!("CREATE SCHEMA {}", quote(d))).map_err(invalid)?;
        ok(dataset_json(&s.project, d))
    })
    .await
}

async fn datasets_get(State(st): State<Shared>, Path((_, d)): Path<(String, String)>) -> Resp {
    blocking(st, move |s| {
        if !s.dataset_exists(&d) {
            return Err(not_found(&format!("Dataset {}:{d}", s.project)));
        }
        ok(dataset_json(&s.project, &d))
    })
    .await
}

async fn datasets_delete(
    State(st): State<Shared>,
    Path((_, d)): Path<(String, String)>,
    Query(q): Query<HashMap<String, String>>,
) -> Resp {
    blocking(st, move |s| {
        if !s.dataset_exists(&d) {
            return Err(not_found(&format!("Dataset {}:{d}", s.project)));
        }
        let cascade = if q.get("deleteContents").is_some_and(|v| v == "true") { " CASCADE" } else { "" };
        s.db.execute_batch(&format!("DROP SCHEMA {}{cascade}", quote(&d))).map_err(invalid)?;
        Ok((StatusCode::NO_CONTENT, Json(Value::Null)))
    })
    .await
}

// ---- tables -----------------------------------------------------------------

async fn tables_list(State(st): State<Shared>, Path((_, d)): Path<(String, String)>) -> Resp {
    blocking(st, move |s| {
        if !s.dataset_exists(&d) {
            return Err(not_found(&format!("Dataset {}:{d}", s.project)));
        }
        let mut stmt = s
            .db
            .prepare(
                "SELECT table_name, table_type FROM information_schema.tables
                 WHERE table_catalog = current_database() AND table_schema = ? ORDER BY 1",
            )
            .map_err(invalid)?;
        let tables: Vec<Value> = stmt
            .query_map([&d], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .map_err(invalid)?
            .filter_map(Result::ok)
            .map(|(t, kind)| {
                json!({
                    "kind": "bigquery#table",
                    "id": format!("{}:{d}.{t}", s.project),
                    "tableReference": table_ref(&s.project, &d, &t),
                    "type": if kind == "VIEW" { "VIEW" } else { "TABLE" },
                })
            })
            .collect();
        ok(json!({"kind": "bigquery#tableList", "totalItems": tables.len(), "tables": tables}))
    })
    .await
}

async fn tables_insert(State(st): State<Shared>, Path((_, d)): Path<(String, String)>, AnyJson(body): AnyJson) -> Resp {
    blocking(st, move |s| {
        let t = body["tableReference"]["tableId"].as_str().ok_or_else(|| invalid("missing tableId"))?;
        if s.table(&d, t).is_some() {
            return Err(err(StatusCode::CONFLICT, "duplicate", &format!("Already Exists: Table {}:{d}.{t} (already exists)", s.project)));
        }
        let name = format!("{}.{}", quote(&d), quote(t));
        let sql = if let Some(view) = body["view"]["query"].as_str() {
            let stmt = translate::translate(view, &[]).map_err(invalid)?.into_iter().next().ok_or_else(|| invalid("empty view"))?;
            format!("CREATE VIEW {name} AS {}", stmt.sql)
        } else {
            // shortcut: timePartitioning / clustering / descriptions are accepted and dropped.
            let cols: Vec<String> = body["schema"]["fields"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|f| format!("{} {}", quote(f["name"].as_str().unwrap_or("")), duck_type_of_field(f)))
                .collect();
            if cols.is_empty() {
                return Err(invalid("smallquery cannot create a table without columns"));
            }
            format!("CREATE TABLE {name} ({})", cols.join(", "))
        };
        s.db.execute_batch(&sql).map_err(invalid)?;
        ok(s.table_json(&d, t)?)
    })
    .await
}

async fn tables_get(State(st): State<Shared>, Path((_, d, t)): Path<(String, String, String)>) -> Resp {
    // shortcut: PATCH/PUT land here too and change nothing (descriptions, labels, expiry).
    blocking(st, move |s| ok(s.table_json(&d, &t)?)).await
}

async fn tables_delete(State(st): State<Shared>, Path((_, d, t)): Path<(String, String, String)>) -> Resp {
    blocking(st, move |s| {
        let (kind, _) = s.table(&d, &t).ok_or_else(|| not_found(&format!("Table {}:{d}.{t}", s.project)))?;
        let what = if kind == "VIEW" { "VIEW" } else { "TABLE" };
        s.db.execute_batch(&format!("DROP {what} {}.{}", quote(&d), quote(&t))).map_err(invalid)?;
        Ok((StatusCode::NO_CONTENT, Json(Value::Null)))
    })
    .await
}

async fn tabledata_list(
    State(st): State<Shared>,
    Path((_, d, t)): Path<(String, String, String)>,
    Query(q): Query<HashMap<String, String>>,
) -> Resp {
    blocking(st, move |s| {
        let (start, max, int64_ts) = page(&q);
        let (_, rows, total) = s.rows(&d, &t, start, max, int64_ts)?;
        let end = start + rows.len();
        let mut v = json!({"kind": "bigquery#tableDataList", "totalRows": total.to_string(), "rows": rows});
        if end < total {
            v["pageToken"] = json!(end.to_string());
        }
        ok(v)
    })
    .await
}

async fn insert_all(
    State(st): State<Shared>,
    Path((_, d, t)): Path<(String, String, String)>,
    AnyJson(body): AnyJson,
) -> Resp {
    blocking(st, move |s| {
        let (_, cols) = s.table(&d, &t).ok_or_else(|| not_found(&format!("Table {}:{d}.{t}", s.project)))?;
        let rows: Vec<&Value> = body["rows"].as_array().into_iter().flatten().map(|r| &r["json"]).collect();
        if rows.is_empty() {
            return ok(json!({"kind": "bigquery#tableDataInsertAllResponse"}));
        }
        // json_transform casts each value to its column's type; unknown keys are ignored, as
        // with ignoreUnknownValues. JSON columns take the raw text, which DuckDB then parses.
        // shortcut: a value of the wrong type becomes NULL instead of an insertErrors entry.
        let structure: serde_json::Map<String, Value> = cols
            .iter()
            .map(|(n, ty)| (n.clone(), json!(if ty == "JSON" { "VARCHAR" } else { ty.as_str() })))
            .collect();
        let lit = |v: String| format!("'{}'", v.replace('\'', "''"));
        let sql = format!(
            "INSERT INTO {}.{} BY NAME SELECT r.* FROM (SELECT unnest(json_transform({}, {})) AS r)",
            quote(&d),
            quote(&t),
            lit(serde_json::to_string(&rows).unwrap()),
            lit(json!([structure]).to_string()),
        );
        s.db.execute_batch(&sql).map_err(invalid)?;
        ok(json!({"kind": "bigquery#tableDataInsertAllResponse"}))
    })
    .await
}
