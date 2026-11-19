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

pub const EXECUTE: &str = "execute";

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExecuteArgs {
    pub code: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ExecuteOutput {
    pub status: ExecuteStatus,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ExecuteStatus {
    NotImplemented,
}

#[derive(Debug, thiserror::Error)]
pub enum ExecuteError {
    #[error("invalid arguments: {0}")]
    InvalidArguments(#[source] serde_json::Error),
    #[error("failed to serialize output: {0}")]
    Output(#[source] serde_json::Error),
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Server;

impl Server {
    fn execute_tool() -> Tool {
        Tool::new(
            EXECUTE,
            "Accepts JavaScript source code. Execution is not implemented yet: the code is never run and the result is always {\"status\": \"not_implemented\"}.",
            rmcp::model::JsonObject::new(),
        )
        .with_input_schema::<ExecuteArgs>()
        .with_output_schema::<ExecuteOutput>()
    }

    fn execute(arguments: Option<rmcp::model::JsonObject>) -> Result<CallToolResult, ExecuteError> {
        let arguments = serde_json::Value::Object(arguments.unwrap_or_default());
        let args: ExecuteArgs =
            serde_json::from_value(arguments).map_err(ExecuteError::InvalidArguments)?;
        tracing::info!(
            code_bytes = args.code.len(),
            "execute called; code not evaluated"
        );
        let output = ExecuteOutput {
            status: ExecuteStatus::NotImplemented,
        };
        let value = serde_json::to_value(&output).map_err(ExecuteError::Output)?;
        Ok(CallToolResult::structured(value))
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
            EXECUTE => Some(Self::execute_tool()),
            _ => None,
        }
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(vec![Self::execute_tool()]))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        match request.name.as_ref() {
            EXECUTE => match Self::execute(request.arguments) {
                Ok(result) => Ok(result.into()),
                Err(error @ ExecuteError::InvalidArguments(_)) => {
                    tracing::warn!(%error, "rejected execute call");
                    Ok(CallToolResult::error(vec![ContentBlock::text(error.to_string())]).into())
                }
                Err(error @ ExecuteError::Output(_)) => {
                    tracing::error!(%error, "execute failed");
                    Err(ErrorData::internal_error(error.to_string(), None))
                }
            },
            name => Err(ErrorData::invalid_params(
                format!("unknown tool: {name}"),
                None,
            )),
        }
    }
}
