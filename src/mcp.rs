use rmcp::ErrorData;
use rmcp::RoleServer;
use rmcp::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::RequestContext;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use serde_json::json;

use crate::js::{self, JsError, Limits, System1};

pub const EXECUTE: &str = "execute";

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
                "Runs JavaScript in an isolated QuickJS sandbox. `code` is the body of an async function: use `await` freely and `return` a JSON-serializable value, which is returned as `result`. `await system1.decide({{ state, questions }})` asks the hosted System 1 Decisions API (`noul`, `choice` or `score` questions) and resolves to `{{ id, model, provider, answers, usage }}`; calls may run concurrently with `Promise.all`, and failures reject with an `Error` whose `kind` names the cause. There is no other network access and no filesystem, environment, or module access; each call gets a fresh context with a {} s deadline and a {} MiB memory limit. Failures return `{{\"error\": {{\"kind\", \"message\"}}}}` with `isError: true`.",
                self.limits.timeout.as_secs(),
                self.limits.memory_limit / (1024 * 1024),
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
                Err(error @ ExecuteError::InvalidArguments(_)) => {
                    tracing::warn!(%error, "rejected execute call");
                    Ok(CallToolResult::error(vec![ContentBlock::text(error.to_string())]).into())
                }
                Err(ExecuteError::Js(error)) => {
                    tracing::info!(kind = error.kind(), %error, "JavaScript execution failed");
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
