//! Connection mode detection step.

use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;
use tracing::debug;
use wfaas::{StepExecutor, StepId, StepResult, WorkflowContext, WorkflowError, WorkflowResult};

use crate::core::{steps::workflow_data::LocalWorkerWorkflowData, ConnectionMode};

/// Try HTTP health check.
async fn try_http_health_check(
    url: &str,
    timeout_secs: u64,
    client: &Client,
) -> Result<(), String> {
    let is_https = url.starts_with("https://");
    let protocol = if is_https { "https" } else { "http" };
    let clean_url = super::strip_protocol(url);
    let health_url = format!("{}://{}/health", protocol, clean_url);

    client
        .get(&health_url)
        .timeout(Duration::from_secs(timeout_secs))
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|e| format!("Health check failed: {}", e))?;

    Ok(())
}

/// Step 1: Detect connection mode by probing HTTP.
pub struct DetectConnectionModeStep;

#[async_trait]
impl StepExecutor<LocalWorkerWorkflowData> for DetectConnectionModeStep {
    async fn execute(
        &self,
        context: &mut WorkflowContext<LocalWorkerWorkflowData>,
    ) -> WorkflowResult<StepResult> {
        let config = &context.data.config;
        let app_context = context
            .data
            .app_context
            .as_ref()
            .ok_or_else(|| WorkflowError::ContextValueNotFound("app_context".to_string()))?;

        debug!(
            "Detecting connection mode for {} (timeout: {}s, max_attempts: {})",
            config.url, config.health_check_timeout_secs, config.max_connection_attempts
        );

        let url = config.url.clone();
        let timeout = config.health_check_timeout_secs;
        let client = &app_context.client;

        let http_result = try_http_health_check(&url, timeout, client).await;

        let connection_mode = match http_result {
            Ok(_) => {
                debug!("{} detected as HTTP", config.url);
                ConnectionMode::Http
            }
            Err(http_err) => {
                return Err(WorkflowError::StepFailed {
                    step_id: StepId::new("detect_connection_mode"),
                    message: format!(
                        "HTTP health check failed for {}: {}",
                        config.url, http_err
                    ),
                });
            }
        };

        context.data.connection_mode = Some(connection_mode);
        Ok(StepResult::Success)
    }

    fn is_retryable(&self, _error: &WorkflowError) -> bool {
        true
    }
}
