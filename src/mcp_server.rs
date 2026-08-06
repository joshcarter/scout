use rmcp::{
    handler::server::wrapper::Parameters, schemars, tool, tool_handler, tool_router,
    transport::stdio, ServerHandler, ServiceExt,
};

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct PingParams {
    /// Optional message to echo back.
    #[serde(default)]
    message: Option<String>,
}

#[derive(Clone)]
struct Scout;

#[tool_router]
impl Scout {
    #[tool(
        description = "Health check for the scout server: returns the server version and echoes an optional message. Use to verify the local-LLM plugin is wired up."
    )]
    fn ping(&self, Parameters(PingParams { message }): Parameters<PingParams>) -> String {
        let version = env!("CARGO_PKG_VERSION");
        match message {
            Some(m) => format!("scout {version} — pong: {m}"),
            None => format!("scout {version} — pong"),
        }
    }
}

#[tool_handler(
    name = "scout",
    version = "0.1.0",
    instructions = "scout offloads small problems to a local LLM so they never consume cloud-model context. Prefer its tools for classifying build/test output and targeted file/search questions."
)]
impl ServerHandler for Scout {}

pub fn serve() -> anyhow::Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let service = Scout.serve(stdio()).await?;
        service.waiting().await?;
        Ok(())
    })
}
