//! `picusbase api` — discover and call the console-api operations documented in
//! `picusbase-docs/api/console-api`. The index is generated from those specs by
//! `picusbase-docs/tools/check_api.py --cli-index src/api_index.json`.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

const INDEX: &str = include_str!("api_index.json");
const METHODS: [&str; 5] = ["GET", "POST", "PUT", "PATCH", "DELETE"];

#[derive(Deserialize)]
struct Index {
    operations: Vec<Operation>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Operation {
    #[serde(rename = "operationId")]
    pub operation_id: String,
    pub method: String,
    pub path: String,
    pub summary: String,
    pub description: String,
    pub status: String,
    pub auth: String,
    pub permissions: Vec<String>,
    pub parameters: Vec<Value>,
    #[serde(rename = "requestBody")]
    pub request_body: Option<Value>,
    pub responses: BTreeMap<String, String>,
}

#[derive(tabled::Tabled)]
pub struct Row {
    pub method: String,
    pub path: String,
    pub operation_id: String,
    pub status: String,
    pub auth: String,
    pub permissions: String,
}

impl From<Operation> for Row {
    fn from(op: Operation) -> Self {
        Row {
            method: op.method,
            path: op.path,
            operation_id: op.operation_id,
            status: op.status,
            auth: op.auth,
            permissions: op.permissions.join(" "),
        }
    }
}

fn operations() -> Result<Vec<Operation>> {
    Ok(serde_json::from_str::<Index>(INDEX)?.operations)
}

pub fn list(method: Option<&str>, filter: Option<&str>) -> Result<Vec<Operation>> {
    let method = method.map(str::to_ascii_uppercase);
    let filter = filter.map(str::to_ascii_lowercase);
    Ok(operations()?
        .into_iter()
        .filter(|op| method.as_deref().map_or(true, |m| op.method == m))
        .filter(|op| {
            filter.as_deref().map_or(true, |f| {
                format!("{} {} {}", op.operation_id, op.path, op.summary)
                    .to_ascii_lowercase()
                    .contains(f)
            })
        })
        .collect())
}

pub fn describe(operation_id: &str) -> Result<Operation> {
    operations()?
        .into_iter()
        .find(|op| op.operation_id == operation_id)
        .ok_or_else(|| anyhow::anyhow!("no operation '{operation_id}' in the API spec: see `picusbase api list`"))
}

/// A documented path template segment (`{ref}`, `{id}`) matches any non-empty segment.
fn segments_match(template: &str, actual: &str) -> bool {
    let (t, a): (Vec<&str>, Vec<&str>) = (template.split('/').collect(), actual.split('/').collect());
    t.len() == a.len()
        && t.iter().zip(&a).all(|(ts, as_)| {
            if ts.starts_with('{') && ts.ends_with('}') {
                !as_.is_empty()
            } else {
                ts == as_
            }
        })
}

/// Resolves METHOD + path to the documented operation and returns it with the path to send
/// (`{ref}` replaced by the project ref) and the query string. Only `implemented` operations are
/// callable: `stub`, `broken`, `planned` and `deprecated` ones are refused.
pub fn resolve(method: &str, raw_path: &str, project_ref: Option<&str>, query: &[String]) -> Result<(Operation, String, String)> {
    let method = method.to_ascii_uppercase();
    if !METHODS.contains(&method.as_str()) {
        bail!("unsupported method '{method}': use one of {}", METHODS.join(", "));
    }
    let (path_part, inline_query) = raw_path.split_once('?').unwrap_or((raw_path, ""));
    let mut path = path_part.trim_end_matches('/').to_string();
    if path.contains("{ref}") {
        let project_ref = project_ref.ok_or_else(|| anyhow::anyhow!("project_ref is required for {{ref}}: --project-ref or PICUSBASE_PROJECT_REF"))?;
        path = path.replace("{ref}", project_ref);
    }
    let op = operations()?
        .into_iter()
        .filter(|op| op.method == method && segments_match(&op.path, &path))
        .max_by_key(|op| op.path.split('/').filter(|s| !s.starts_with('{')).count())
        .ok_or_else(|| anyhow::anyhow!("{method} {path} is not in the API spec: see `picusbase api list --method {method}`"))?;
    if op.status != "implemented" {
        bail!("{} {} ({}) has status '{}': not callable through the CLI", op.method, op.path, op.operation_id, op.status);
    }
    let mut pairs: Vec<&str> = inline_query.split('&').filter(|p| !p.is_empty()).collect();
    pairs.extend(query.iter().map(String::as_str));
    let query_string = pairs.join("&");
    Ok((op, path, query_string))
}
