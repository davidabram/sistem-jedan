use rmcp::ErrorData;
use rmcp::RoleServer;
use rmcp::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, Implementation, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::RequestContext;
use schemars::JsonSchema;
use serde::Serialize;
use std::sync::Arc;
use tokio::sync::Semaphore;

use serde_json::json;

use crate::diagnostic;
use crate::js::{self, JsError, Limits, System1};

const EXECUTE: &str = "execute";
const MAX_SOURCE_BYTES: usize = 1024 * 1024;
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

Execution failures return `{"error": {"kind", "message"}}` with isError: true, where kind is one of invalid_arguments, exception, timeout, out_of_memory, missing_return, unsupported_result, result_too_large, engine, json or execution_capacity."#;

#[derive(JsonSchema)]
#[schemars(deny_unknown_fields)]
struct ExecuteArgs {
    #[schemars(rename = "code")]
    _code: String,
}

#[derive(Debug, Serialize, JsonSchema)]
struct ExecuteOutput {
    result: serde_json::Value,
}

#[derive(Debug, thiserror::Error)]
enum ExecuteError {
    #[error("invalid arguments: expected only a string `code` field")]
    InvalidArguments,
    #[error("JavaScript source exceeds the {MAX_SOURCE_BYTES} byte limit")]
    SourceTooLarge,
    #[error("all execution slots are busy")]
    AtCapacity,
    #[error(transparent)]
    Js(#[from] JsError),
}

impl ExecuteError {
    fn kind(&self) -> &'static str {
        match self {
            Self::InvalidArguments | Self::SourceTooLarge => "invalid_arguments",
            Self::AtCapacity => "execution_capacity",
            Self::Js(error) => error.kind(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Server {
    limits: Limits,
    system1: Arc<System1>,
    executions: Arc<Semaphore>,
    max_concurrent_executions: usize,
}

impl Server {
    #[must_use]
    pub fn new(limits: Limits, system1: System1, max_concurrent_executions: usize) -> Self {
        Self {
            limits,
            system1: Arc::new(system1),
            executions: Arc::new(Semaphore::new(max_concurrent_executions)),
            max_concurrent_executions,
        }
    }

    fn execute_tool(&self) -> Tool {
        Tool::new(
            EXECUTE,
            format!(
                "{EXECUTE_DESCRIPTION}\n\nLimits: a {} s deadline and a {} MiB QuickJS memory limit per execution, at most {} simultaneous executions (extra calls are rejected), {} system1.decide() calls per execution, and at most {MAX_DECISION_INPUT_MIB} MiB of serialized JSON per decide() argument. JavaScript source is limited to {MAX_SOURCE_BYTES} UTF-8 bytes. The returned JSON result is limited to {MAX_EXECUTE_RESULT_MIB} MiB of serialized UTF-8; larger results fail with `result_too_large`. Filter or aggregate data in JavaScript before returning it.",
                self.limits.timeout.as_secs(),
                self.limits.memory_limit / (1024 * 1024),
                self.max_concurrent_executions,
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
        cancelled: impl Fn() -> bool + Send + 'static,
    ) -> Result<CallToolResult, ExecuteError> {
        let code = arguments
            .as_ref()
            .filter(|fields| fields.len() == 1)
            .and_then(|fields| fields.get("code"))
            .and_then(serde_json::Value::as_str)
            .ok_or(ExecuteError::InvalidArguments)?;
        if code.len() > MAX_SOURCE_BYTES {
            return Err(ExecuteError::SourceTooLarge);
        }
        let _permit = self
            .executions
            .try_acquire()
            .map_err(|_| ExecuteError::AtCapacity)?;
        tracing::info!(code_bytes = code.len(), "executing JavaScript");
        let result = js::run(code, self.limits, Arc::clone(&self.system1), cancelled).await?;
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

    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListToolsResult, ErrorData>> {
        std::future::ready(Ok(ListToolsResult::with_all_items(vec![
            self.execute_tool(),
        ])))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        match request.name.as_ref() {
            EXECUTE => {
                let cancellation = context.ct.clone();
                let result = tokio::select! {
                    biased;
                    () = context.ct.cancelled() => return Err(ErrorData::internal_error("execution cancelled", None)),
                    result = self.execute(request.arguments, move || cancellation.is_cancelled()) => {
                        if context.ct.is_cancelled() {
                            return Err(ErrorData::internal_error("execution cancelled", None));
                        }
                        result
                    },
                };
                match result {
                    Ok(result) => Ok(result.into()),
                    Err(error) => {
                        let kind = error.kind();
                        let message = diagnostic::message(error);
                        tracing::info!(kind, %message, "execute failed");
                        Ok(CallToolResult::structured_error(json!({
                            "error": { "kind": kind, "message": message }
                        }))
                        .into())
                    }
                }
            }
            name => Err(ErrorData::invalid_params(
                diagnostic::message(format_args!("unknown tool: {name}")),
                None,
            )),
        }
    }
}
