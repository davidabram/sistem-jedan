use rmcp::ErrorData;
use rmcp::RoleServer;
use rmcp::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, Implementation, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::RequestContext;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use serde_json::json;

use crate::js::{self, JsError, Limits, System1};

pub const EXECUTE: &str = "execute";
const MAX_DECISION_INPUT_MIB: usize = js::MAX_DECISION_INPUT_BYTES / (1024 * 1024);
const MAX_EXECUTE_RESULT_MIB: usize = js::MAX_EXECUTE_RESULT_BYTES / (1024 * 1024);
const EXECUTE_DESCRIPTION: &str = r#"Runs JavaScript in a fresh, isolated QuickJS runtime. `code` is the body of an async function: top-level `await` works, and you must explicitly `return` a JSON-serializable value, which comes back as `{"result": value}`. Execution ends when the returned promise settles, so await or return every promise whose outcome matters; detached promises are not guaranteed to finish and their rejections are ignored. There is no filesystem, environment, timers, modules or network access other than system1.decide().

system1.decide({ state, questions }) asks the configured System 1 model (Decisions API). The endpoint, model and API key are server configuration, never arguments. Unknown fields are rejected.
- state: string, object or array (nested at most 64 levels).
- questions: object of 1 or more named questions; names are 1-64 ASCII letters, digits, `_` or `-`. Instructions and criteria descriptions must not be blank.
  - {type: "noul", instructions?, criteria?: {true: "...", false: "..."}} -> {type: "noul", noul: probability 0..1, confidence?}
  - {type: "choice", instructions?, criteria: {id: "description", ...} (at least 2)} -> {type: "choice", choice: id, probabilities?: {id: p}, confidence?}
  - {type: "score", instructions?, criteria: ["lowest", ..., "highest"] (at least 2, ordered)} -> {type: "score", score: number 0..criteria.length-1 (zero-indexed), probabilities?: {"0": p, ...}, legend?: {"0": "...", ...}, confidence?}
  - Any question may instead be answered {type: "refusal"}; check `answer.type` before reading type-specific fields.
Resolves to {id, model, provider, answers, usage}: `answers` is keyed by your question names; `usage` has input_tokens, output_tokens and optional cost details.
Example: const d = await system1.decide({ state: diff, questions: { risky: { type: "noul", instructions: "Is this change risky?" } } }); const a = d.answers.risky; return a.type === "noul" ? a.noul : null;

A failed decision rejects with an Error named "System1Error" whose `kind` is one of provider_not_configured, call_limit_exceeded, invalid_input, timeout, transport, authentication, http, invalid_response, encode or limiter_closed. Invalid or oversized input does not consume the call budget. Use try/catch, Promise.all for concurrent calls and Promise.allSettled to collect individual failures; an uncaught rejection fails the execution with kind `exception`.

Execution failures return `{"error": {"kind", "message"}}` with isError: true, where kind is one of invalid_arguments, exception, timeout, out_of_memory, never_settled, missing_return, unsupported_result, result_too_large, engine or json."#;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExecuteArgs {
    pub code: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ExecuteOutput {
    pub result: serde_json::Value,
}

#[derive(Debug, thiserror::Error)]
pub enum ExecuteError {
    #[error("invalid arguments: {0}")]
    InvalidArguments(#[source] serde_json::Error),
    #[error(transparent)]
    Js(#[from] JsError),
}

impl ExecuteError {
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::InvalidArguments(_) => "invalid_arguments",
            Self::Js(error) => error.kind(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Server {
    limits: Limits,
    system1: Arc<System1>,
}

impl Server {
    #[must_use]
    pub fn new(limits: Limits, system1: System1) -> Self {
        Self {
            limits,
            system1: Arc::new(system1),
        }
    }

    fn execute_tool(&self) -> Tool {
        Tool::new(
            EXECUTE,
            format!(
                "{EXECUTE_DESCRIPTION}\n\nLimits: a {} s deadline and a {} MiB QuickJS heap per execution, at most {} system1.decide() calls per execution, and at most {MAX_DECISION_INPUT_MIB} MiB of serialized JSON per decide() argument. The returned JSON result is limited to {MAX_EXECUTE_RESULT_MIB} MiB of serialized UTF-8; larger results fail with `result_too_large`. Filter or aggregate data in JavaScript before returning it.",
                self.limits.timeout.as_secs(),
                self.limits.memory_limit / (1024 * 1024),
                self.system1.max_calls(),
            ),
            rmcp::model::JsonObject::new(),
        )
        .with_input_schema::<ExecuteArgs>()
        .with_output_schema::<ExecuteOutput>()
    }

    async fn execute(
        &self,
        arguments: Option<rmcp::model::JsonObject>,
    ) -> Result<CallToolResult, ExecuteError> {
        let arguments = serde_json::Value::Object(arguments.unwrap_or_default());
        let args: ExecuteArgs =
            serde_json::from_value(arguments).map_err(ExecuteError::InvalidArguments)?;
        tracing::info!(code_bytes = args.code.len(), "executing JavaScript");
        let result = js::run(&args.code, self.limits, Arc::clone(&self.system1)).await?;
        Ok(CallToolResult::structured(json!(ExecuteOutput { result })))
    }
}

impl ServerHandler for Server {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build()).with_server_info(
            Implementation::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")),
        )
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        match name {
            EXECUTE => Some(self.execute_tool()),
            _ => None,
        }
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(vec![self.execute_tool()]))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        match request.name.as_ref() {
            EXECUTE => match self.execute(request.arguments).await {
                Ok(result) => Ok(result.into()),
                Err(error) => {
                    if let ExecuteError::InvalidArguments(_) = error {
                        tracing::warn!(%error, "rejected execute call");
                    } else {
                        tracing::info!(kind = error.kind(), %error, "JavaScript execution failed");
                    }
                    Ok(CallToolResult::structured_error(json!({
                        "error": { "kind": error.kind(), "message": error.to_string() }
                    }))
                    .into())
                }
            },
            name => Err(ErrorData::invalid_params(
                format!("unknown tool: {name}"),
                None,
            )),
        }
    }
}
