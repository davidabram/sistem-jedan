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

use crate::config::{BYTES_PER_MIB, ExecutionSettings};
use crate::decisions::{
    Client, MAX_NESTING_DEPTH, MAX_QUESTION_NAME_LEN, MIN_CHOICES, MIN_SCORE_LEVELS,
};
use crate::diagnostic;
use crate::js::{
    self, CALL_LIMIT_KIND, INVALID_INPUT_KIND, JsError, OUT_OF_MEMORY_KIND,
    PROVIDER_NOT_CONFIGURED_KIND, SYSTEM1_ERROR, System1, TIMEOUT_KIND,
};

const EXECUTE: &str = "execute";
const MAX_SOURCE_BYTES: usize = BYTES_PER_MIB;
const MAX_DECISION_INPUT_MIB: usize = js::MAX_DECISION_INPUT_BYTES / BYTES_PER_MIB;
const MAX_EXECUTE_RESULT_MIB: usize = js::MAX_EXECUTE_RESULT_BYTES / BYTES_PER_MIB;

#[derive(JsonSchema)]
#[schemars(deny_unknown_fields)]
struct ExecuteArgs {
    #[schemars(rename = "code")]
    _code: String,
}

#[derive(Serialize, JsonSchema)]
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
    const fn kind(&self) -> &'static str {
        match self {
            Self::InvalidArguments | Self::SourceTooLarge => "invalid_arguments",
            Self::AtCapacity => "execution_capacity",
            Self::Js(error) => error.kind(),
        }
    }
}

pub(super) struct Server {
    settings: ExecutionSettings,
    system1: Arc<System1>,
    executions: Semaphore,
}

impl Server {
    pub(super) fn new(settings: ExecutionSettings, client: Option<Client>) -> Self {
        let system1 = System1::new(
            client,
            settings.max_concurrent_requests,
            settings.max_calls_per_execution,
        );
        Self {
            executions: Semaphore::new(settings.max_concurrent_executions),
            system1: Arc::new(system1),
            settings,
        }
    }

    fn execute_tool(&self) -> Tool {
        let settings = &self.settings;
        Tool::new(
            EXECUTE,
            format!(
                r#"Runs the UTF-8 `code` (up to {MAX_SOURCE_BYTES} bytes) as the body of an async function in a fresh QuickJS runtime. Use top-level `await` and explicitly `return` a JSON-serializable value. The response is {{"result": value}}. Await or return every promise whose outcome matters. Detached promises may never finish. Scripts have no filesystem, environment, module, timer or network access except `system1.decide()`.

`system1.decide({{state, questions}})` uses the configured Decisions API. Unknown fields are rejected.
- State is a string, object or array, nested at most {MAX_NESTING_DEPTH} levels.
- Supply one or more named questions. Names use 1–{MAX_QUESTION_NAME_LEN} ASCII letters, digits, underscores or hyphens. Instructions and criteria descriptions must be nonblank.
- `noul`: optional criteria `{{true: "...", false: "..."}}`. Answer has `noul` (probability 0–1).
- `choice`: criteria map at least {MIN_CHOICES} IDs to descriptions. Answer has `choice` (an ID) and optional `probabilities`.
- `score`: criteria list at least {MIN_SCORE_LEVELS} descriptions in ascending order. Answer has `score` (0 to criteria.length − 1), optional `probabilities` and `legend` with canonical decimal keys such as "0".
- Each question takes optional `instructions`. Answers may include `confidence`. Always check `answer.type` for `refusal` before reading type-specific fields.

The decision resolves to `{{id, model, provider, answers, usage}}`, with answers keyed by question name. Example:
`const d = await system1.decide({{state: "Checkout fails in Safari", questions: {{bug: {{type: "noul"}}}}}}); const a = d.answers.bug; return a.type === "noul" ? a.noul : null;`

A decision can reject with a `{SYSTEM1_ERROR}` whose `kind` explains the failure, including `{PROVIDER_NOT_CONFIGURED_KIND}`, `{CALL_LIMIT_KIND}`, `{INVALID_INPUT_KIND}` and provider failures. Invalid input spends no calls. Run independent decisions with Promise.all and use try/catch or Promise.allSettled to keep partial results. An uncaught decision rejection fails the execution.

Limits per execution are {timeout} s, {memory} MiB of QuickJS backing allocations, {calls} decision calls, {MAX_DECISION_INPUT_MIB} MiB of JSON per decision input and {MAX_EXECUTE_RESULT_MIB} MiB of JSON for the result. At most {executions} executions and {requests} inference requests run concurrently. Inference waits count toward the deadline.

Execution errors return `{{"error": {{"kind", "message"}}}}` with `isError: true`. Excess executions fail immediately with `{capacity}`. Pending promises and running code both fail with `{TIMEOUT_KIND}` on deadline expiry. Allocator exhaustion fails with `{OUT_OF_MEMORY_KIND}`. Use an explicit return to avoid `{missing}` and filter or aggregate results to avoid `{too_large}`."#,
                timeout = settings.limits.timeout.as_secs(),
                memory = settings.limits.memory_limit.get() / BYTES_PER_MIB,
                calls = settings.max_calls_per_execution,
                executions = settings.max_concurrent_executions,
                requests = settings.max_concurrent_requests,
                capacity = ExecuteError::AtCapacity.kind(),
                missing = JsError::MissingReturn.kind(),
                too_large = JsError::ResultTooLarge.kind(),
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
        let result = js::run(
            code,
            self.settings.limits,
            Arc::clone(&self.system1),
            cancelled,
        )
        .await?;
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
