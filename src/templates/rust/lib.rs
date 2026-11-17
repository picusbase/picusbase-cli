use picusbase_function_sdk::{Error, ExecutionContext, HttpHandler, HttpRequest, HttpResponse};
use serde_json::{json, Value};
use std::collections::HashMap;

pub struct {{STRUCT_NAME}};

impl HttpHandler for {{STRUCT_NAME}} {
    fn handle(req: HttpRequest, _ctx: ExecutionContext) -> Result<HttpResponse, Error> {
        let received = req.body.unwrap_or(Value::Null);
        Ok(HttpResponse {
            status: 200,
            headers: HashMap::new(),
            body: json!({ "ok": true, "received": received }),
        })
    }
}
