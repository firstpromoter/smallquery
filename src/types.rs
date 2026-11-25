//! BigQuery <-> DuckDB types, and DuckDB values -> BigQuery's REST row format.
//!
//! BigQuery TIMESTAMP is an instant, so it maps to TIMESTAMPTZ (the session runs
//! in UTC); DATETIME is a wall-clock value, so it maps to plain TIMESTAMP.

use serde_json::{Value as Json, json};

/// A GoogleSQL type name (`INT64`, `ARRAY<STRING>`, `STRUCT<a INT64>`, `NUMERIC(10,2)`) -> DuckDB.
pub fn duck_type(t: &str) -> String {
    let t = t.trim();
    let up = t.to_uppercase();
    if up.starts_with("ARRAY<") && up.ends_with('>') {
        return format!("{}[]", duck_type(&t[6..t.len() - 1]));
    }
    if up.starts_with("STRUCT<") && up.ends_with('>') {
        let fields: Vec<String> = split_top(&t[7..t.len() - 1])
            .iter()
            .map(|f| match f.trim().split_once(char::is_whitespace) {
                Some((name, ty)) => format!("{name} {}", duck_type(ty)),
                None => duck_type(f), // anonymous field
            })
            .collect();
        return format!("STRUCT({})", fields.join(", "));
    }
    let (base, args) = match up.split_once('(') {
        Some((b, a)) => (b.trim(), Some(a.trim_end_matches(')'))),
        None => (up.as_str(), None),
    };
    match base {
        "NUMERIC" | "BIGNUMERIC" | "DECIMAL" | "BIGDECIMAL" => match args {
            Some(a) => format!("DECIMAL({a})"),
            None => "DECIMAL(38,9)".into(),
        },
        "STRING" => "VARCHAR".into(),
        "BYTES" => "BLOB".into(),
        "INT64" | "INTEGER" | "INT" | "SMALLINT" | "BIGINT" | "TINYINT" | "BYTEINT" => "BIGINT".into(),
        "FLOAT64" | "FLOAT" => "DOUBLE".into(),
        "BOOL" | "BOOLEAN" => "BOOLEAN".into(),
        "DATETIME" => "TIMESTAMP".into(),
        "TIMESTAMP" => "TIMESTAMPTZ".into(),
        _ => base.into(), // DATE, TIME, JSON, INTERVAL
    }
}

/// A REST schema field (`{name, type, mode, fields}`) -> DuckDB column type.
pub fn duck_type_of_field(f: &Json) -> String {
    let ty = f["type"].as_str().unwrap_or("STRING").to_uppercase();
    let base = if ty == "RECORD" || ty == "STRUCT" {
        let inner: Vec<String> = f["fields"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|c| format!("{} {}", quote(c["name"].as_str().unwrap_or("")), duck_type_of_field(c)))
            .collect();
        format!("STRUCT({})", inner.join(", "))
    } else {
        duck_type(&ty)
    };
    if f["mode"].as_str() == Some("REPEATED") { format!("{base}[]") } else { base }
}

/// A DuckDB type string (as information_schema / DESCRIBE print it) -> a REST schema field.
pub fn bq_field(name: &str, duck: &str) -> Json {
    let duck = duck.trim();
    if let Some(elem) = duck.strip_suffix("[]") {
        let mut f = bq_field(name, elem);
        f["mode"] = json!("REPEATED");
        return f;
    }
    if duck.to_uppercase().starts_with("STRUCT(") {
        let fields: Vec<Json> = split_top(&duck[7..duck.len() - 1])
            .iter()
            .map(|f| {
                let (n, t) = split_name(f.trim());
                bq_field(&n, t)
            })
            .collect();
        return json!({"name": name, "type": "RECORD", "mode": "NULLABLE", "fields": fields});
    }
    let up = duck.to_uppercase();
    let base = up.split('(').next().unwrap_or("");
    let ty = match base {
        "TINYINT" | "SMALLINT" | "INTEGER" | "BIGINT" | "HUGEINT" | "UTINYINT" | "USMALLINT" | "UINTEGER"
        | "UBIGINT" | "UHUGEINT" => "INTEGER",
        "FLOAT" | "DOUBLE" => "FLOAT",
        "DECIMAL" => "NUMERIC",
        "BOOLEAN" => "BOOLEAN",
        "DATE" => "DATE",
        "TIMESTAMP WITH TIME ZONE" | "TIMESTAMPTZ" => "TIMESTAMP",
        "TIMESTAMP" | "TIMESTAMP_S" | "TIMESTAMP_MS" | "TIMESTAMP_NS" => "DATETIME",
        "TIME" => "TIME",
        "JSON" => "JSON",
        "BLOB" => "BYTES",
        "INTERVAL" => "INTERVAL",
        _ => "STRING",
    };
    json!({"name": name, "type": ty, "mode": "NULLABLE"})
}

/// SQL that renders column `col` (of DuckDB type `duck`) as the string BigQuery puts on the wire.
pub fn wire_expr(col: &str, duck: &str) -> String {
    let c = quote(col);
    let up = duck.to_uppercase();
    if up.ends_with("[]") || up.starts_with("STRUCT(") || up.starts_with("MAP(") {
        // shortcut: nested values go through JSON, so timestamps inside arrays/structs keep
        // DuckDB's text form instead of BigQuery's epoch form; fine until a report returns one.
        return format!("to_json({c})::VARCHAR");
    }
    match up.as_str() {
        "TIMESTAMP WITH TIME ZONE" | "TIMESTAMPTZ" => format!("epoch_us({c})::VARCHAR"),
        "TIMESTAMP" => format!("strftime({c}, '%Y-%m-%dT%H:%M:%S.%f')"),
        "BLOB" => format!("base64({c})"),
        _ => format!("{c}::VARCHAR"),
    }
}

/// Wire string from `wire_expr` -> the `{"v": ...}` cell for field `f`.
pub fn cell(f: &Json, raw: Option<String>, int64_ts: bool) -> Json {
    let Some(s) = raw else { return json!({"v": null}) };
    if f["mode"] == "REPEATED" || f["type"] == "RECORD" {
        let parsed: Json = serde_json::from_str(&s).unwrap_or(Json::Null);
        return nested(f, &parsed);
    }
    if f["type"] == "TIMESTAMP" && !int64_ts {
        let us: i64 = s.parse().unwrap_or(0);
        let sign = if us < 0 { "-" } else { "" };
        let a = us.unsigned_abs();
        return json!({"v": format!("{sign}{}.{:06}", a / 1_000_000, a % 1_000_000)});
    }
    json!({"v": s})
}

fn nested(f: &Json, v: &Json) -> Json {
    if v.is_null() {
        return json!({"v": null});
    }
    if f["mode"] == "REPEATED" {
        let mut elem = f.clone();
        elem["mode"] = json!("NULLABLE");
        let items: Vec<Json> = v.as_array().into_iter().flatten().map(|x| nested(&elem, x)).collect();
        return json!({"v": items});
    }
    if f["type"] == "RECORD" {
        let cells: Vec<Json> = f["fields"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|sub| nested(sub, &v[sub["name"].as_str().unwrap_or("")]))
            .collect();
        return json!({"v": {"f": cells}});
    }
    match v {
        Json::String(s) => json!({"v": s}),
        other => json!({"v": other.to_string()}),
    }
}

pub fn quote(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

/// Split on commas that are not nested inside (), <> or quotes.
fn split_top(s: &str) -> Vec<String> {
    let (mut out, mut cur, mut depth, mut quoted) = (vec![], String::new(), 0i32, false);
    for ch in s.chars() {
        match ch {
            '"' => quoted = !quoted,
            '(' | '<' if !quoted => depth += 1,
            ')' | '>' if !quoted => depth -= 1,
            ',' if depth == 0 && !quoted => {
                out.push(std::mem::take(&mut cur));
                continue;
            }
            _ => {}
        }
        cur.push(ch);
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out
}

/// `"a b" INTEGER` / `a INTEGER` -> (name, type)
fn split_name(f: &str) -> (String, &str) {
    if let Some(rest) = f.strip_prefix('"') {
        let end = rest.find('"').unwrap_or(rest.len());
        return (rest[..end].to_string(), rest[end + 1..].trim());
    }
    match f.split_once(' ') {
        Some((n, t)) => (n.to_string(), t.trim()),
        None => (f.to_string(), "VARCHAR"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_types_both_ways() {
        assert_eq!(duck_type("ARRAY<STRUCT<a INT64, b STRING>>"), "STRUCT(a BIGINT, b VARCHAR)[]");
        assert_eq!(duck_type("NUMERIC(10, 2)"), "DECIMAL(10, 2)");
        assert_eq!(duck_type("timestamp"), "TIMESTAMPTZ");
        let f = bq_field("x", "STRUCT(a INTEGER, \"b c\" TIMESTAMP WITH TIME ZONE)[]");
        assert_eq!(f["mode"], "REPEATED");
        assert_eq!(f["fields"][1]["name"], "b c");
        assert_eq!(f["fields"][1]["type"], "TIMESTAMP");
    }

    #[test]
    fn encodes_timestamps_as_epoch_seconds() {
        let f = json!({"type": "TIMESTAMP", "mode": "NULLABLE"});
        assert_eq!(cell(&f, Some("1704103200500000".into()), false)["v"], "1704103200.500000");
        assert_eq!(cell(&f, Some("1704103200500000".into()), true)["v"], "1704103200500000");
    }
}
