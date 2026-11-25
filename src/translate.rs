//! GoogleSQL -> DuckDB SQL.
//!
//! Parse with sqlparser's BigQuery dialect, rewrite the AST bottom-up, print.
//! Rewrites that need a different shape are written as DuckDB SQL templates and
//! re-parsed, so each rule reads like the SQL it produces. Reference for semantics:
//! https://cloud.google.com/bigquery/docs/reference/standard-sql/functions-and-operators

use std::ops::ControlFlow;

use serde_json::Value as Json;
use sqlparser::ast::*;
use sqlparser::dialect::{BigQueryDialect, DuckDbDialect};
use sqlparser::keywords::Keyword;
use sqlparser::parser::Parser;
use sqlparser::tokenizer::{Token, Tokenizer, Word};

use crate::types::{duck_type, quote};

pub struct Stmt {
    pub sql: String,
    pub statement_type: &'static str,
    /// (dataset, table) the statement creates, for CREATE TABLE / VIEW.
    pub target: Option<(String, String)>,
}

pub fn translate(sql: &str, params: &[Json]) -> Result<Vec<Stmt>, String> {
    let tokens = Tokenizer::new(&BigQueryDialect, sql).tokenize().map_err(|e| e.to_string())?;
    let mut statements = Parser::new(&BigQueryDialect)
        .with_tokens(requote(tokens))
        .parse_statements()
        .map_err(|e| e.to_string())?;
    let mut rw = Rewriter { params, positional: 0 };
    statements
        .iter_mut()
        .map(|s| {
            if let ControlFlow::Break(e) = s.visit(&mut rw) {
                return Err(e);
            }
            let (statement_type, target) = kind(s);
            Ok(Stmt { sql: s.to_string(), statement_type, target })
        })
        .collect()
}

/// `` `project.dataset.table` `` is one token in BigQuery; DuckDB wants `"project"."dataset"."table"`.
/// The project part resolves because the database is attached under the project's name.
fn requote(tokens: Vec<Token>) -> Vec<Token> {
    let mut out = Vec::with_capacity(tokens.len());
    for t in tokens {
        match t {
            Token::Word(w) if w.quote_style == Some('`') => {
                for (i, part) in w.value.split('.').enumerate() {
                    if i > 0 {
                        out.push(Token::Period);
                    }
                    out.push(Token::Word(Word { value: part.into(), quote_style: Some('"'), keyword: Keyword::NoKeyword }));
                }
            }
            t => out.push(t),
        }
    }
    out
}

fn kind(s: &Statement) -> (&'static str, Option<(String, String)>) {
    match s {
        Statement::Query(_) => ("SELECT", None),
        Statement::CreateTable(ct) => {
            (if ct.query.is_some() { "CREATE_TABLE_AS_SELECT" } else { "CREATE_TABLE" }, Some(name_parts(&ct.name)))
        }
        Statement::CreateView(v) => ("CREATE_VIEW", Some(name_parts(&v.name))),
        Statement::Insert(_) => ("INSERT", None),
        Statement::Update { .. } => ("UPDATE", None),
        Statement::Delete(_) => ("DELETE", None),
        Statement::Merge { .. } => ("MERGE", None),
        Statement::Truncate { .. } => ("TRUNCATE_TABLE", None),
        Statement::Drop { .. } => ("DROP_TABLE", None),
        Statement::CreateSchema { .. } => ("CREATE_SCHEMA", None),
        _ => ("SCRIPT", None),
    }
}

fn name_parts(n: &ObjectName) -> (String, String) {
    let parts: Vec<String> = n
        .0
        .iter()
        .filter_map(|p| match p {
            ObjectNamePart::Identifier(i) => Some(i.value.clone()),
            _ => None,
        })
        .collect();
    let len = parts.len();
    let dataset = if len >= 2 { parts[len - 2].clone() } else { String::new() };
    (dataset, parts.last().cloned().unwrap_or_default())
}

struct Rewriter<'a> {
    params: &'a [Json],
    positional: usize,
}

impl VisitorMut for Rewriter<'_> {
    type Break = String;

    fn post_visit_expr(&mut self, e: &mut Expr) -> ControlFlow<String> {
        match self.rewrite(e) {
            Ok(Some(new)) => *e = new,
            Ok(None) => {}
            Err(m) => return ControlFlow::Break(m),
        }
        ControlFlow::Continue(())
    }

    fn post_visit_value(&mut self, v: &mut ValueWithSpan) -> ControlFlow<String> {
        // GoogleSQL "..." and '''...''' / """...""" are strings; in DuckDB "..." is an identifier.
        if let Value::DoubleQuotedString(s) | Value::TripleDoubleQuotedString(s) | Value::TripleSingleQuotedString(s) =
            &v.value
        {
            v.value = Value::SingleQuotedString(s.clone());
        }
        ControlFlow::Continue(())
    }

    fn post_visit_select(&mut self, s: &mut Select) -> ControlFlow<String> {
        // GROUP BY name: BigQuery resolves a SELECT alias first; DuckDB calls it ambiguous when a
        // FROM column shares the name. Grouping by the aliased expression means the same thing.
        if let GroupByExpr::Expressions(exprs, _) = &mut s.group_by {
            for g in exprs.iter_mut() {
                let Expr::Identifier(id) = g else { continue };
                let aliased = s.projection.iter().find_map(|p| match p {
                    SelectItem::ExprWithAlias { expr, alias } if alias.value.eq_ignore_ascii_case(&id.value) => Some(expr),
                    _ => None,
                });
                if let Some(expr) = aliased {
                    *g = expr.clone();
                }
            }
        }
        // BigQuery names an unaliased expression column f0_, f1_, ... by position.
        for (i, item) in s.projection.iter_mut().enumerate() {
            if let SelectItem::UnnamedExpr(e) = item {
                if !matches!(e, Expr::Identifier(_) | Expr::CompoundIdentifier(_)) {
                    *item = SelectItem::ExprWithAlias { expr: e.clone(), alias: Ident::new(format!("f{i}_")) };
                }
            }
        }
        // SELECT * EXCEPT (a) -> SELECT * EXCLUDE (a)
        for item in &mut s.projection {
            if let SelectItem::ExprWithAlias { alias, .. } = item {
                quote_keyword(alias);
            }
            if let SelectItem::Wildcard(o) | SelectItem::QualifiedWildcard(_, o) = item {
                if let Some(ex) = o.opt_except.take() {
                    let cols = std::iter::once(ex.first_element)
                        .chain(ex.additional_elements)
                        .map(|i| ObjectName::from(vec![i]))
                        .collect();
                    o.opt_exclude = Some(ExcludeSelectItem::Multiple(cols));
                }
            }
        }
        ControlFlow::Continue(())
    }

    fn post_visit_table_factor(&mut self, tf: &mut TableFactor) -> ControlFlow<String> {
        if let TableFactor::Table { name, alias, .. } = tf {
            let Some(sql) = information_schema(name, alias.as_ref()) else { return ControlFlow::Continue(()) };
            match Parser::new(&DuckDbDialect).try_with_sql(&sql).and_then(|mut p| p.parse_table_factor()) {
                Ok(new) => *tf = new,
                Err(e) => return ControlFlow::Break(format!("smallquery: cannot rewrite INFORMATION_SCHEMA: {e}")),
            }
            return ControlFlow::Continue(());
        }
        // FROM t, UNNEST(arr) AS x [WITH OFFSET AS i]
        //   -> FROM t, (SELECT unnest(arr) AS x [, generate_subscripts(arr, 1) - 1 AS i]) AS _unnest_x
        let TableFactor::UNNEST { alias, array_exprs, with_offset, with_offset_alias, .. } = tf else {
            return ControlFlow::Continue(());
        };
        let Some(arr) = array_exprs.first() else { return ControlFlow::Continue(()) };
        let col = alias
            .as_ref()
            .map(|a| a.columns.first().map(|c| c.name.value.clone()).unwrap_or(a.name.value.clone()))
            .unwrap_or_else(|| "unnest".into());
        let mut sql = format!("(SELECT unnest({arr}) AS {}", quote(&col));
        if *with_offset {
            let off = with_offset_alias.as_ref().map(|i| i.value.clone()).unwrap_or_else(|| "offset".into());
            sql += &format!(", generate_subscripts({arr}, 1) - 1 AS {}", quote(&off));
        }
        sql += &format!(") AS {}", quote(&format!("_unnest_{col}")));
        match Parser::new(&DuckDbDialect).try_with_sql(&sql).and_then(|mut p| p.parse_table_factor()) {
            Ok(new) => *tf = new,
            Err(e) => return ControlFlow::Break(format!("smallquery: cannot rewrite UNNEST: {e}")),
        }
        ControlFlow::Continue(())
    }

    fn post_visit_statement(&mut self, s: &mut Statement) -> ControlFlow<String> {
        // BigQuery storage/table options mean nothing to DuckDB.
        match s {
            Statement::CreateTable(ct) => {
                ct.partition_by = None;
                ct.cluster_by = None;
                ct.table_options = CreateTableOptions::None;
                for c in &mut ct.columns {
                    c.data_type = custom_type(&c.data_type);
                    c.options.retain(|o| !matches!(o.option, ColumnOption::Options(_)));
                }
            }
            Statement::CreateView(v) => v.options = CreateTableOptions::None,
            _ => {}
        }
        ControlFlow::Continue(())
    }
}

impl Rewriter<'_> {
    fn rewrite(&mut self, e: &mut Expr) -> Result<Option<Expr>, String> {
        let sql = match e {
            Expr::Value(v) => match &v.value {
                Value::Placeholder(p) => self.param(p)?,
                _ => return Ok(None),
            },
            // BigQuery's tokenizer reads `@name` as an identifier.
            Expr::Identifier(i) if i.quote_style.is_none() && i.value.starts_with('@') => self.param(&i.value)?,
            Expr::Identifier(i) => {
                quote_keyword(i);
                return Ok(None);
            }
            Expr::CompoundIdentifier(parts) => {
                parts.iter_mut().for_each(quote_keyword);
                return Ok(None);
            }
            Expr::Cast { kind, data_type, .. } => {
                if *kind == CastKind::SafeCast {
                    *kind = CastKind::TryCast;
                }
                *data_type = custom_type(data_type);
                return Ok(None);
            }
            Expr::TypedString(ts) => {
                ts.data_type = custom_type(&ts.data_type);
                return Ok(None);
            }
            Expr::In { expr, target: InTarget::Unnest(array_expr), negated, .. } => {
                format!("{expr} {}IN (SELECT unnest({array_expr}))", if *negated { "NOT " } else { "" })
            }
            Expr::Struct { values, fields } => {
                let items: Vec<String> = values
                    .iter()
                    .enumerate()
                    .map(|(i, v)| {
                        let (val, name) = match v {
                            Expr::Named { expr, name } => (expr.to_string(), name.value.clone()),
                            v => (
                                v.to_string(),
                                fields
                                    .get(i)
                                    .and_then(|f| f.field_name.as_ref().map(|n| n.value.clone()))
                                    .unwrap_or(format!("_field_{}", i + 1)),
                            ),
                        };
                        let val = match fields.get(i) {
                            Some(f) => format!("CAST({val} AS {})", duck_type(&f.field_type.to_string())),
                            None => val,
                        };
                        format!("{} := {val}", quote(&name))
                    })
                    .collect();
                format!("struct_pack({})", items.join(", "))
            }
            Expr::CompoundFieldAccess { access_chain, .. } => {
                // arr[OFFSET(i)] (0-based) / arr[ORDINAL(i)] (1-based) -> DuckDB's 1-based arr[i]
                for a in access_chain.iter_mut() {
                    if let AccessExpr::Subscript(Subscript::Index { index }) = a {
                        if let Expr::Function(f) = index {
                            let name = f.name.to_string().to_uppercase();
                            let args = args_of(f);
                            if args.len() == 1 {
                                let i = &args[0];
                                *index = match name.as_str() {
                                    "OFFSET" | "SAFE_OFFSET" => parse(&format!("({i}) + 1"))?,
                                    "ORDINAL" | "SAFE_ORDINAL" => parse(i)?,
                                    _ => continue,
                                };
                            }
                        }
                    }
                }
                return Ok(None);
            }
            Expr::Extract { field: DateTimeField::Custom(f), expr, .. } if f.value.eq_ignore_ascii_case("DAYOFWEEK") => {
                format!("(dayofweek({expr}) + 1)") // BigQuery: Sunday = 1
            }
            Expr::Function(f) if f.name.to_string().eq_ignore_ascii_case("ARRAY_AGG") => {
                // ARRAY_AGG(x IGNORE NULLS ORDER BY y LIMIT n) -> list_slice(array_agg(x ORDER BY y) FILTER (x IS NOT NULL), 1, n)
                let FunctionArguments::List(l) = &mut f.args else { return Ok(None) };
                let mut limit = None;
                let mut ignore_nulls = false;
                l.clauses.retain(|c| match c {
                    FunctionArgumentClause::Limit(n) => {
                        limit = Some(n.to_string());
                        false
                    }
                    FunctionArgumentClause::IgnoreOrRespectNulls(n) => {
                        ignore_nulls = *n == NullTreatment::IgnoreNulls;
                        false
                    }
                    _ => true,
                });
                if ignore_nulls && f.filter.is_none() {
                    let arg = args_of(f).into_iter().next().unwrap_or_default();
                    f.filter = Some(Box::new(parse(&format!("({arg}) IS NOT NULL"))?));
                }
                match limit {
                    Some(n) => format!("list_slice({f}, 1, {n})"),
                    None => return Ok(None),
                }
            }
            Expr::Function(f) => {
                let name = f.name.to_string().to_uppercase();
                match function(&name, &args_of(f))? {
                    Some(sql) => sql,
                    None => return Ok(None),
                }
            }
            _ => return Ok(None),
        };
        parse(&sql).map(Some)
    }

    /// `@name` / `?` -> a typed literal built from the request's queryParameters.
    fn param(&mut self, p: &str) -> Result<String, String> {
        let found = if let Some(name) = p.strip_prefix('@') {
            self.params.iter().find(|x| x["name"].as_str().is_some_and(|n| n.eq_ignore_ascii_case(name)))
        } else {
            self.positional += 1;
            self.params.get(self.positional - 1)
        };
        let found = found.ok_or(format!("Query parameter '{p}' not found"))?;
        literal(&found["parameterType"], &found["parameterValue"])
    }
}

/// `[project.]dataset.INFORMATION_SCHEMA.{COLUMNS,TABLES}` and `[region.]INFORMATION_SCHEMA.SCHEMATA`
/// -> DuckDB's information_schema, filtered to that dataset, with GoogleSQL type names
/// (`bq_type` is a macro installed at startup).
fn information_schema(name: &ObjectName, alias: Option<&TableAlias>) -> Option<String> {
    let parts: Vec<String> = name.0.iter().map(|p| p.to_string().trim_matches('"').to_string()).collect();
    let n = parts.len();
    if n < 2 || !parts[n - 2].eq_ignore_ascii_case("INFORMATION_SCHEMA") {
        return None;
    }
    let view = parts[n - 1].to_uppercase();
    let dataset = (n >= 3).then(|| format!(" AND table_schema = '{}'", parts[n - 3].replace('\'', "''")));
    let alias = alias.map(|a| a.to_string()).unwrap_or_else(|| quote(&view.to_lowercase()));
    let body = match view.as_str() {
        "COLUMNS" => format!(
            "SELECT table_catalog, table_schema, table_name, column_name, ordinal_position, is_nullable, \
             bq_type(data_type) AS data_type FROM information_schema.columns \
             WHERE table_catalog = current_database(){}",
            dataset.unwrap_or_default()
        ),
        "TABLES" => format!(
            "SELECT table_catalog, table_schema, table_name, \
             CASE table_type WHEN 'VIEW' THEN 'VIEW' ELSE 'BASE TABLE' END AS table_type \
             FROM information_schema.tables WHERE table_catalog = current_database(){}",
            dataset.unwrap_or_default()
        ),
        "SCHEMATA" => "SELECT catalog_name, schema_name FROM information_schema.schemata \
             WHERE catalog_name = current_database() AND schema_name NOT IN ('main', '_smallquery')"
            .to_string(),
        _ => return None,
    };
    Some(format!("({body}) AS {alias}"))
}

fn literal(ty: &Json, val: &Json) -> Result<String, String> {
    let t = ty["type"].as_str().unwrap_or("STRING").to_uppercase();
    if t == "ARRAY" {
        let items: Result<Vec<String>, String> = val["arrayValues"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|v| literal(&ty["arrayType"], v))
            .collect();
        let elem = duck_type(ty["arrayType"]["type"].as_str().unwrap_or("STRING"));
        return Ok(format!("CAST([{}] AS {elem}[])", items?.join(", ")));
    }
    if t == "STRUCT" {
        return Err("smallquery: STRUCT query parameters are not supported".into());
    }
    Ok(match &val["value"] {
        Json::Null => format!("CAST(NULL AS {})", duck_type(&t)),
        Json::String(s) => format!("CAST('{}' AS {})", s.replace('\'', "''"), duck_type(&t)),
        other => format!("CAST('{other}' AS {})", duck_type(&t)),
    })
}

fn args_of(f: &Function) -> Vec<String> {
    match &f.args {
        FunctionArguments::List(l) => l
            .args
            .iter()
            .map(|a| match a {
                FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => e.to_string(),
                other => other.to_string(),
            })
            .collect(),
        _ => vec![],
    }
}

/// GoogleSQL function -> DuckDB SQL. None = same name and meaning in DuckDB.
fn function(name: &str, a: &[String]) -> Result<Option<String>, String> {
    let arg = |i: usize, default: &str| a.get(i).cloned().unwrap_or(default.into());
    Ok(Some(match (name, a.len()) {
        ("DATE", 1) => format!("CAST({} AS DATE)", a[0]),
        ("DATE", 3) => format!("make_date({}, {}, {})", a[0], a[1], a[2]),
        ("DATETIME", 1) => format!("CAST({} AS TIMESTAMP)", a[0]),
        ("TIMESTAMP", 1) => format!("CAST({} AS TIMESTAMPTZ)", a[0]),
        ("DATE_ADD", 2) => format!("CAST(({}) + {} AS DATE)", a[0], a[1]),
        ("DATE_SUB", 2) => format!("CAST(({}) - {} AS DATE)", a[0], a[1]),
        ("DATETIME_ADD" | "TIMESTAMP_ADD", 2) => format!("(({}) + {})", a[0], a[1]),
        ("DATETIME_SUB" | "TIMESTAMP_SUB", 2) => format!("(({}) - {})", a[0], a[1]),
        // shortcut: WEEK diffs use DuckDB's ISO (Monday) weeks, BigQuery's are Sunday-based.
        ("DATE_DIFF", 3) => format!("date_diff('{}', {}, {})", part(&a[2]), a[1], a[0]),
        ("DATETIME_DIFF" | "TIMESTAMP_DIFF", 3) => format!("date_sub('{}', {}, {})", part(&a[2]), a[1], a[0]),
        ("DATE_TRUNC", 2) => format!("CAST({} AS DATE)", trunc(&a[0], &a[1])?),
        ("DATETIME_TRUNC" | "TIMESTAMP_TRUNC", 2) => trunc(&a[0], &a[1])?,
        ("LAST_DAY", 1) => format!("last_day({})", a[0]),
        ("CURRENT_DATE", 0) => "current_date".into(),
        ("CURRENT_TIMESTAMP", 0) => "get_current_timestamp()".into(),
        ("CURRENT_DATETIME", 0) => "CAST(get_current_timestamp() AS TIMESTAMP)".into(),
        ("PARSE_DATE", 2) => format!("CAST(strptime({}, {}) AS DATE)", a[1], a[0]),
        ("PARSE_DATETIME", 2) => format!("CAST(strptime({}, {}) AS TIMESTAMP)", a[1], a[0]),
        ("PARSE_TIMESTAMP", 2) => format!("CAST(strptime({}, {}) AS TIMESTAMPTZ)", a[1], a[0]),
        ("FORMAT_DATE" | "FORMAT_DATETIME" | "FORMAT_TIMESTAMP", 2) => format!("strftime({}, {})", a[1], a[0]),
        ("UNIX_SECONDS", 1) => format!("CAST(epoch({}) AS BIGINT)", a[0]),
        ("UNIX_MILLIS", 1) => format!("epoch_ms({})", a[0]),
        ("UNIX_MICROS", 1) => format!("epoch_us({})", a[0]),
        ("TIMESTAMP_SECONDS", 1) => format!("to_timestamp({})", a[0]),
        ("GENERATE_DATE_ARRAY", 2 | 3) => format!(
            "CAST(generate_series(CAST({} AS DATE), CAST({} AS DATE), {}) AS DATE[])",
            a[0],
            a[1],
            arg(2, "INTERVAL 1 DAY")
        ),
        ("COUNTIF", 1) => format!("count_if({})", a[0]),
        ("LOGICAL_OR", 1) => format!("bool_or({})", a[0]),
        ("LOGICAL_AND", 1) => format!("bool_and({})", a[0]),
        ("SAFE_DIVIDE", 2) => format!("(CASE WHEN ({1}) = 0 THEN NULL ELSE ({0}) / ({1}) END)", a[0], a[1]),
        ("DIV", 2) => format!("(({}) // ({}))", a[0], a[1]),
        // shortcut: goes through DOUBLE, so it can be off in the last digit for >15 significant digits.
        ("TRUNC", 2) => format!("(trunc(({0}) * pow(10, {1})) / pow(10, {1}))", a[0], a[1]),
        ("SPLIT", 1 | 2) => format!("string_split({}, {})", a[0], arg(1, "','")),
        ("ARRAY_LENGTH", 1) => format!("len({})", a[0]),
        ("JSON_VALUE" | "JSON_EXTRACT_SCALAR", 1 | 2) => format!("json_extract_string({}, {})", a[0], arg(1, "'$'")),
        ("JSON_QUERY" | "JSON_EXTRACT", 1 | 2) => format!("json_extract({}, {})", a[0], arg(1, "'$'")),
        ("JSON_QUERY_ARRAY" | "JSON_EXTRACT_ARRAY", 1 | 2) => {
            format!("CAST(json_extract({}, {}) AS JSON[])", a[0], arg(1, "'$'"))
        }
        ("TO_JSON_STRING", 1) => format!("CAST(to_json({}) AS VARCHAR)", a[0]),
        ("REGEXP_CONTAINS", 2) => format!("regexp_matches({}, {})", a[0], a[1]),
        ("GENERATE_UUID", 0) => "CAST(uuid() AS VARCHAR)".into(),
        _ => return Ok(None),
    }))
}

/// `MONTH`, `WEEK`, `WEEK(MONDAY)`, `ISOWEEK` -> DuckDB date_trunc.
fn trunc(x: &str, p: &str) -> Result<String, String> {
    Ok(match p.to_uppercase().replace(' ', "").as_str() {
        "WEEK" | "WEEK(SUNDAY)" => format!("(date_trunc('week', ({x}) + INTERVAL 1 DAY) - INTERVAL 1 DAY)"),
        "WEEK(MONDAY)" | "ISOWEEK" => format!("date_trunc('week', {x})"),
        w if w.starts_with("WEEK(") => return Err(format!("smallquery: DATE_TRUNC to {w} is not supported")),
        other => format!("date_trunc('{}', {x})", part(other)),
    })
}

fn part(p: &str) -> String {
    p.trim().to_lowercase()
}

/// DuckDB keywords that cannot be a bare column name (`SELECT duckdb_keywords()` where the category
/// is not 'unreserved'), minus the ones BigQuery reserves too, which arrive quoted already.
const DUCKDB_KEYWORDS: &[&str] = &[
    "ANALYSE", "ANALYZE", "ANTI", "ASOF", "ASYMMETRIC", "AT", "AUTHORIZATION", "BIGINT", "BINARY", "BIT",
    "BOOLEAN", "BOTH", "CHAR", "CHARACTER", "CHECK", "COALESCE", "COLLATION", "COLUMN", "COLUMNS",
    "CONCURRENTLY", "CONSTRAINT", "DEC", "DECIMAL", "DEFERRABLE", "DESCRIBE", "DO", "EXTRACT", "FLOAT",
    "FOREIGN", "FREEZE", "GENERATED", "GLOB", "GROUPING_ID", "ILIKE", "INITIALLY", "INOUT", "INT", "INTEGER",
    "ISNULL", "LAMBDA", "LEADING", "LIMIT", "MAP", "NATIONAL", "NCHAR", "NONE", "NOTNULL", "NULLIF", "NUMERIC",
    "OFFSET", "ONLY", "OUT", "OVERLAPS", "OVERLAY", "PIVOT", "PIVOT_LONGER", "PIVOT_WIDER", "PLACING",
    "POSITION", "POSITIONAL", "PRECISION", "PRIMARY", "QUALIFY", "REAL", "REFERENCES", "RETURNING", "ROW",
    "SEMI", "SETOF", "SHOW", "SIMILAR", "SMALLINT", "STRUCT", "SUBSTRING", "SUMMARIZE", "SYMMETRIC", "TABLE",
    "TABLESAMPLE", "TIME", "TIMESTAMP", "TRAILING", "TREAT", "TRIM", "TRY_CAST", "TUPLE", "UNIQUE", "UNPACK",
    "UNPIVOT", "VALUES", "VARCHAR", "VARIADIC", "VERBOSE",
];

/// A column or alias named like a DuckDB keyword (`at`, `time`, `values`) must be quoted there.
fn quote_keyword(i: &mut Ident) {
    if i.quote_style.is_none() && DUCKDB_KEYWORDS.iter().any(|k| k.eq_ignore_ascii_case(&i.value)) {
        i.quote_style = Some('"');
    }
}

fn custom_type(dt: &DataType) -> DataType {
    DataType::Custom(ObjectName::from(vec![Ident::new(duck_type(&dt.to_string()))]), vec![])
}

fn parse(sql: &str) -> Result<Expr, String> {
    Parser::new(&DuckDbDialect)
        .try_with_sql(sql)
        .and_then(|mut p| p.parse_expr())
        .map_err(|e| format!("smallquery: bad rewrite `{sql}`: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(sql: &str) -> String {
        translate(sql, &[]).unwrap().remove(0).sql
    }

    #[test]
    fn rewrites_googlesql() {
        assert_eq!(
            t("SELECT DATE_SUB(CURRENT_DATE(), INTERVAL 2 DAY) FROM `p.d.t`"),
            "SELECT CAST((current_date) - INTERVAL 2 DAY AS DATE) AS f0_ FROM \"p\".\"d\".\"t\""
        );
        assert_eq!(t("SELECT SPLIT(url, '?')[OFFSET(0)] FROM x"), "SELECT string_split(url, '?')[(0) + 1] AS f0_ FROM x");
        assert_eq!(t("SELECT SAFE_CAST(a AS INT64) FROM x"), "SELECT TRY_CAST(a AS BIGINT) AS f0_ FROM x");
        assert_eq!(t("SELECT * EXCEPT (a) FROM x"), "SELECT * EXCLUDE (a) FROM x");
        assert_eq!(
            t("SELECT \"it's\" AS s, DATE \"2026-03-01\" AS d FROM x"),
            "SELECT 'it''s' AS s, DATE '2026-03-01' AS d FROM x"
        );
        assert_eq!(t("SELECT at, x.time AS values FROM x"), "SELECT \"at\", x.\"time\" AS \"values\" FROM x");
        assert_eq!(
            t("SELECT 1 FROM x WHERE id IN UNNEST([1, 2])"),
            "SELECT 1 AS f0_ FROM x WHERE id IN (SELECT unnest([1, 2]))"
        );
        assert_eq!(
            t("SELECT DATE_TRUNC(DATE(created_at), WEEK(MONDAY)) FROM x"),
            "SELECT CAST(date_trunc('week', CAST(created_at AS DATE)) AS DATE) AS f0_ FROM x"
        );
    }

    #[test]
    fn rewrites_unnest_with_offset() {
        assert_eq!(
            t("SELECT item, i FROM e CROSS JOIN UNNEST(e.items) AS item WITH OFFSET AS i"),
            "SELECT item, i FROM e CROSS JOIN (SELECT unnest(e.items) AS \"item\", generate_subscripts(e.items, 1) - 1 AS \"i\") AS \"_unnest_item\""
        );
    }

    #[test]
    fn strips_table_options_and_reports_target() {
        let s = translate(
            "create or replace table `p`.`d`.`t` partition by created_at cluster by a, b OPTIONS() as (select 1 as a)",
            &[],
        )
        .unwrap()
        .remove(0);
        assert_eq!(s.statement_type, "CREATE_TABLE_AS_SELECT");
        assert_eq!(s.target, Some(("d".into(), "t".into())));
        assert!(!s.sql.to_uppercase().contains("PARTITION"), "{}", s.sql);
    }

    #[test]
    fn binds_parameters_as_literals() {
        let params = serde_json::json!([
            {"name": "ids", "parameterType": {"type": "ARRAY", "arrayType": {"type": "STRING"}},
             "parameterValue": {"arrayValues": [{"value": "1"}, {"value": "o'k"}]}},
            {"name": "d", "parameterType": {"type": "DATE"}, "parameterValue": {"value": "2024-01-01"}}
        ]);
        let s = translate("SELECT 1 FROM x WHERE d >= @d AND id IN UNNEST(@ids)", params.as_array().unwrap())
            .unwrap()
            .remove(0)
            .sql;
        assert!(s.contains("CAST('2024-01-01' AS DATE)"), "{s}");
        assert!(s.contains("'o''k'"), "{s}");
    }
}
