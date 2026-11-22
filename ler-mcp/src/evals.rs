//! Runs evals/questions.json against a real database: each case makes the tool call a good
//! answer would make and checks the structured result. Ignored by default (needs the database):
//!
//!   LER_DB=../lers_analysis.db cargo test evals -- --ignored --nocapture
//!
//! This tests the tools, not the model: whether Claude picks these calls for these questions is
//! checked by hand, asking `question` in a session with the server connected.

use std::path::PathBuf;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use serde::Deserialize;
use serde_json::Value;

use crate::LerServer;
use crate::db::Db;

#[derive(Debug, Deserialize)]
struct EvalFile {
    cases: Vec<Case>,
}

#[derive(Debug, Deserialize)]
struct Case {
    id: String,
    question: String,
    call: Call,
    checks: Vec<Check>,
}

#[derive(Debug, Deserialize)]
struct Call {
    tool: String,
    args: Value,
}

#[derive(Debug, Deserialize)]
struct Check {
    #[serde(default)]
    path: String,
    op: Op,
    value: Value,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Op {
    Eq,
    Contains,
    OneOf,
    Min,
    Max,
    Len,
    LenMin,
    ErrorContains,
}

/// Every value at a JSON pointer, where a `*` segment fans out over an array.
fn select<'a>(v: &'a Value, path: &str) -> Vec<&'a Value> {
    let mut cur = vec![v];
    for seg in path.split('/').skip(1) {
        cur = cur
            .into_iter()
            .flat_map(|v| -> Vec<&Value> {
                match (seg, v) {
                    ("*", Value::Array(a)) => a.iter().collect(),
                    (_, Value::Array(a)) => seg
                        .parse()
                        .ok()
                        .and_then(|i: usize| a.get(i))
                        .into_iter()
                        .collect(),
                    (_, Value::Object(o)) => o.get(seg).into_iter().collect(),
                    _ => vec![],
                }
            })
            .collect();
    }
    cur
}

/// Checks one value; Err says what was found instead.
fn check_value(op: Op, got: &Value, want: &Value) -> Result<(), String> {
    let ok = match op {
        Op::Eq => got == want,
        Op::Contains => match (got, want) {
            (Value::String(g), Value::String(w)) => g.to_lowercase().contains(&w.to_lowercase()),
            (Value::Array(g), w) => g.contains(w),
            _ => false,
        },
        Op::OneOf => want.as_array().is_some_and(|a| a.contains(got)),
        Op::Min => got.as_f64().zip(want.as_f64()).is_some_and(|(g, w)| g >= w),
        Op::Max => got.as_f64().zip(want.as_f64()).is_some_and(|(g, w)| g <= w),
        Op::Len => got.as_array().map(|a| a.len() as u64) == want.as_u64(),
        Op::LenMin => got
            .as_array()
            .zip(want.as_u64())
            .is_some_and(|(a, w)| a.len() as u64 >= w),
        Op::ErrorContains => false, // handled in `check`
    };
    if ok {
        return Ok(());
    }
    let shown = match (op, got) {
        (Op::Len | Op::LenMin, Value::Array(a)) => format!("{} items", a.len()),
        _ => {
            let s = got.to_string();
            s.chars().take(120).collect()
        }
    };
    Err(format!("{op:?} {want} failed: got {shown}"))
}

/// Runs one check against a tool's outcome (structured content, or the error text).
fn check(c: &Check, outcome: &Result<Value, String>) -> Result<(), String> {
    match (c.op, outcome) {
        (Op::ErrorContains, Err(e)) => {
            let w = c.value.as_str().unwrap_or_default();
            e.contains(w).then_some(()).ok_or_else(|| {
                let head: String = e.chars().take(160).collect();
                format!("error {head:?}... lacks {w:?}")
            })
        }
        (Op::ErrorContains, Ok(_)) => Err("expected an error, the call succeeded".into()),
        (_, Err(e)) => Err(format!("tool error: {e}")),
        (op, Ok(v)) => {
            let found = select(v, &c.path);
            if found.is_empty() {
                return Err(format!("{} not found", c.path));
            }
            found
                .into_iter()
                .try_for_each(|g| check_value(op, g, &c.value))
                .map_err(|e| format!("{}: {e}", c.path))
        }
    }
}

fn args<T: serde::de::DeserializeOwned>(v: &Value) -> Result<Parameters<T>, String> {
    serde_json::from_value(v.clone())
        .map(Parameters)
        .map_err(|e| format!("bad args: {e}"))
}

/// Calls a tool the way the MCP router would, returning its structured content.
async fn call(server: &LerServer, c: &Call) -> Result<Value, String> {
    let result: CallToolResult = match c.tool.as_str() {
        "get_event" => server.get_event(args(&c.args)?).await?,
        "get_event_text" => server.get_event_text(args(&c.args)?).await?,
        "search_events" => server.search_events(args(&c.args)?).await?,
        "summarize_events" => server.summarize_events(args(&c.args)?).await?,
        "list_precursors" => server.list_precursors(args(&c.args)?).await?,
        other => return Err(format!("unknown tool {other}")),
    };
    result
        .structured_content
        .ok_or_else(|| "no structured content".into())
}

fn db_path() -> PathBuf {
    std::env::var_os("LER_DB")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../lers_analysis.db"))
        })
}

#[tokio::test]
#[ignore = "needs lers_analysis.db; run with --ignored"]
async fn evals() {
    let file: EvalFile = serde_json::from_str(include_str!("../evals/questions.json")).unwrap();
    let server = LerServer::new(Db::open(&db_path()).expect("open LER_DB"));
    println!("{}\n", server.db.info());
    let mut failed = 0;
    for case in &file.cases {
        let outcome = call(&server, &case.call).await;
        let errors: Vec<String> = case
            .checks
            .iter()
            .filter_map(|c| check(c, &outcome).err())
            .collect();
        let status = if errors.is_empty() { "pass" } else { "FAIL" };
        println!("{status}  {:<26} {}", case.id, case.question);
        for e in &errors {
            println!("        {e}");
        }
        failed += usize::from(!errors.is_empty());
    }
    println!(
        "\n{} of {} passed",
        file.cases.len() - failed,
        file.cases.len()
    );
    assert_eq!(failed, 0, "{failed} eval case(s) failed");
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ck(path: &str, op: Op, value: Value) -> Check {
        Check {
            path: path.into(),
            op,
            value,
        }
    }

    #[test]
    fn select_follows_pointers_and_stars() {
        let v = json!({"results": [{"site": "A"}, {"site": "B"}], "total": 2});
        assert_eq!(select(&v, "/total"), [&json!(2)]);
        assert_eq!(select(&v, "/results/1/site"), [&json!("B")]);
        assert_eq!(select(&v, "/results/*/site"), [&json!("A"), &json!("B")]);
        assert!(select(&v, "/missing").is_empty());
    }

    #[test]
    fn checks_pass_and_fail() {
        let ok: Result<Value, String> =
            Ok(json!({"results": [{"site": "Sequoyah"}], "total": 108}));
        assert!(check(&ck("/results/*/site", Op::Eq, json!("Sequoyah")), &ok).is_ok());
        assert!(check(&ck("/total", Op::Min, json!(100)), &ok).is_ok());
        assert!(check(&ck("/total", Op::Max, json!(100)), &ok).is_err());
        assert!(check(&ck("/results", Op::Len, json!(1)), &ok).is_ok());
        assert!(check(&ck("/results/0/site", Op::Contains, json!("sequoyah")), &ok).is_ok());
        assert!(
            check(
                &ck("/results/0/site", Op::OneOf, json!(["X", "Sequoyah"])),
                &ok
            )
            .is_ok()
        );
        assert!(check(&ck("", Op::ErrorContains, json!("x")), &ok).is_err());
        let err: Result<Value, String> = Err("no plant matches \"Springfield\"".into());
        assert!(check(&ck("", Op::ErrorContains, json!("no plant")), &err).is_ok());
        assert!(check(&ck("/total", Op::Min, json!(1)), &err).is_err());
    }

    #[test]
    fn question_file_parses() {
        let f: EvalFile = serde_json::from_str(include_str!("../evals/questions.json")).unwrap();
        assert!(f.cases.len() >= 10);
        for c in &f.cases {
            assert!(!c.checks.is_empty(), "{} has no checks", c.id);
        }
    }
}
