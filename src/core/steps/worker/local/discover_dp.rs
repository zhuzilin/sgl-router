//! Data Parallel (DP) information discovery step.

use async_trait::async_trait;
use tracing::debug;
use wfaas::{StepExecutor, StepId, StepResult, WorkflowContext, WorkflowError, WorkflowResult};

use super::discover_metadata::get_server_info;
use crate::core::{steps::workflow_data::LocalWorkerWorkflowData, UNKNOWN_MODEL_ID};

/// DP (Data Parallel) information for a worker.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DpInfo {
    pub dp_size: usize,
    pub model_id: String,
}

/// Get DP info for a worker URL.
pub async fn get_dp_info(url: &str, api_key: Option<&str>) -> Result<DpInfo, String> {
    let info = get_server_info(url, api_key).await?;

    let dp_size = info
        .dp_size
        .ok_or_else(|| format!("No dp_size in response from {}", url))?;

    let model_id = info
        .model_id
        .filter(|s| !s.is_empty())
        .or(info.served_model_name.filter(|s| !s.is_empty()))
        .or_else(|| {
            info.model_path
                .and_then(|path| path.split('/').next_back().map(|s| s.to_string()))
        })
        .unwrap_or_else(|| UNKNOWN_MODEL_ID.to_string());

    Ok(DpInfo { dp_size, model_id })
}

/// Check if discovered labels indicate dp_attention is enabled with dp_size > 1.
fn is_dp_attention_detected(labels: &std::collections::HashMap<String, String>) -> bool {
    let dp_attention_enabled = labels
        .get("enable_dp_attention")
        .map(|v| v == "true")
        .unwrap_or(false);
    let dp_size = labels
        .get("dp_size")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(1);
    dp_attention_enabled && dp_size > 1
}

/// Step 2b: Discover DP (Data Parallel) information (only for DP-aware workers).
pub struct DiscoverDPInfoStep;

#[async_trait]
impl StepExecutor<LocalWorkerWorkflowData> for DiscoverDPInfoStep {
    async fn execute(
        &self,
        context: &mut WorkflowContext<LocalWorkerWorkflowData>,
    ) -> WorkflowResult<StepResult> {
        let config = &context.data.config;

        // Auto-detect dp_attention from discovered metadata labels
        let dp_attention_detected = is_dp_attention_detected(&context.data.discovered_labels);

        if !config.dp_aware && !dp_attention_detected {
            debug!(
                "Worker {} is not DP-aware and dp_attention not detected, skipping DP discovery",
                config.url
            );
            return Ok(StepResult::Success);
        }

        if dp_attention_detected {
            debug!(
                "Auto-detected dp_attention for {} (dp_size: {})",
                config.url,
                context
                    .data
                    .discovered_labels
                    .get("dp_size")
                    .unwrap_or(&"?".to_string())
            );
        } else {
            debug!("Discovering DP info for {} (DP-aware)", config.url);
        }

        let dp_info = get_dp_info(&config.url, config.api_key.as_deref())
            .await
            .map_err(|e| WorkflowError::StepFailed {
                step_id: StepId::new("discover_dp_info"),
                message: format!("Failed to get DP info: {}", e),
            })?;

        debug!(
            "Discovered DP size {} for {} (model: {})",
            dp_info.dp_size, config.url, dp_info.model_id
        );

        context.data.dp_info = Some(dp_info);
        Ok(StepResult::Success)
    }

    fn is_retryable(&self, _error: &WorkflowError) -> bool {
        true
    }
}
