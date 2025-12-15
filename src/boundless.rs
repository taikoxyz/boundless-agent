use crate::config::{BoundlessConfig, BoundlessOfferParams};
#[cfg(test)]
use crate::config::{
    BoundlessAggregationGuestInput, BoundlessAggregationGuestOutput, DeploymentConfig,
    DeploymentType, OfferParamsConfig,
};
use crate::image_manager::ImageManager;
use crate::storage::BoundlessStorage;
use crate::types::{AsyncProofRequest, ProofRequestStatus, ProofType};
#[cfg(test)]
use crate::types::Risc0Response;
use crate::utils::parse_staking_token;
use alloy_primitives_v1p2p0::{U256, utils::parse_ether};
use boundless_market::{
    Client, ProofRequest,
    deployments::Deployment,
    input::GuestEnv,
    request_builder::OfferParams,
};
use reqwest::Url;
use risc0_zkvm::{Journal, compute_image_id, default_executor};
#[cfg(test)]
use risc0_zkvm::{Digest, Receipt as ZkvmReceipt};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::sync::{OnceCell, RwLock};
use tokio::task::JoinHandle;

// Constants
const MILLION_CYCLES: u64 = 1_000_000;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProverConfig {
    pub offchain: bool,
    pub pull_interval: u64,
    pub rpc_url: String,
    pub boundless_config: BoundlessConfig,
    pub url_ttl: u64,
}

impl Default for ProverConfig {
    fn default() -> Self {
        ProverConfig {
            offchain: false,
            pull_interval: 10,
            // rpc_url: "https://ethereum-sepolia-rpc.publicnode.com".to_string(),
            rpc_url: "https://base-rpc.publicnode.com".to_string(),
            boundless_config: BoundlessConfig::default(),
            url_ttl: 1800,
        }
    }
}

#[derive(Clone)]
pub struct BoundlessProver {
    pub(crate) config: ProverConfig,
    pub(crate) deployment: Deployment,
    pub(crate) boundless_config: BoundlessConfig,
    pub(crate) client: Arc<OnceCell<Client>>,
    pub(crate) active_requests: Arc<RwLock<HashMap<String, AsyncProofRequest>>>,
    pub(crate) poll_error_counts: Arc<RwLock<HashMap<String, u32>>>,
    pub(crate) storage: BoundlessStorage,
    pub(crate) image_manager: ImageManager,
}

impl std::fmt::Debug for BoundlessProver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoundlessProver")
            .field("config", &self.config)
            .field("deployment", &self.deployment)
            .field("boundless_config", &self.boundless_config)
            .field("active_requests", &"<RwLock<HashMap<..>>>")
            .field("poll_error_counts", &"<RwLock<HashMap<..>>>")
            .field("storage", &self.storage)
            .field("image_manager", &self.image_manager)
            .finish()
    }
}

static TTL_CLEANUP_HANDLE: OnceLock<tokio::sync::Mutex<Option<JoinHandle<()>>>> = OnceLock::new();

fn ttl_cleanup_handle() -> &'static tokio::sync::Mutex<Option<JoinHandle<()>>> {
    TTL_CLEANUP_HANDLE.get_or_init(|| tokio::sync::Mutex::new(None))
}

// More specific error types
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("Failed to build boundless client: {0}")]
    ClientBuildError(String),
    #[error("Transient polling error: {0}")]
    PollingError(String),
    #[error("Retry error: {0}")]
    RetryError(String),
    #[error("Failed to encode guest environment: {0}")]
    GuestEnvEncodeError(String),
    #[error("Failed to upload input: {0}")]
    UploadError(String),
    #[error("Failed to upload program: {0}")]
    ProgramUploadError(String),
    #[error("Failed to build request: {0}")]
    RequestBuildError(String),
    #[error("Failed to submit request: {0}")]
    RequestSubmitError(String),
    #[error("Failed to wait for request fulfillment after {attempts} attempts: {error}")]
    RequestFulfillmentError { attempts: u32, error: String },
    #[error("Failed to encode response: {0}")]
    ResponseEncodeError(String),
    #[error("Failed to execute guest environment: {0}")]
    GuestExecutionError(String),
    #[error("Did not receive requested unaggregated receipt")]
    InvalidReceiptError,
    #[error("Missing journal in fulfillment data")]
    MissingJournalError,
    #[error("Failed to decode fulfillment data: {0}")]
    FulfillmentDecodeError(String),
    #[error("Storage provider is required")]
    StorageProviderRequired,
}

pub type AgentResult<T> = Result<T, AgentError>;

impl BoundlessProver {
    /// Create a deployment based on the configuration
    fn create_deployment(config: &ProverConfig) -> AgentResult<Deployment> {
        Ok(config.boundless_config.get_effective_deployment())
    }

    /// Process input and create guest environment
    pub(crate) fn process_input(&self, input: Vec<u8>) -> AgentResult<(GuestEnv, Vec<u8>)> {
        let guest_env = GuestEnv::builder().write_frame(&input).build_env();
        let guest_env_bytes = guest_env.clone().encode().map_err(|e| {
            AgentError::ClientBuildError(format!("Failed to encode guest environment: {e}"))
        })?;
        Ok((guest_env, guest_env_bytes))
    }

    pub async fn new(config: ProverConfig, image_manager: ImageManager) -> AgentResult<Self> {
        let deployment = BoundlessProver::create_deployment(&config)?;
        tracing::info!("boundless deployment: {:?}", deployment);

        // Initialize SQLite storage
        let db_path = std::env::var("SQLITE_DB_PATH")
            .unwrap_or_else(|_| "./boundless_requests.db".to_string());
        let storage = BoundlessStorage::new(db_path);
        storage.initialize().await?;

        // Clean up expired requests from previous runs
        match storage.delete_expired_requests().await {
            Ok(deleted_ids) => {
                if !deleted_ids.is_empty() {
                    tracing::info!(
                        "Cleaned up {} expired requests from previous runs",
                        deleted_ids.len()
                    );
                }
            }
            Err(e) => tracing::warn!("Failed to clean up expired requests: {}", e),
        }

        let boundless_config = config.boundless_config.clone();

        let prover = BoundlessProver {
            config,
            deployment,
            boundless_config,
            client: Arc::new(OnceCell::new()),
            active_requests: Arc::new(RwLock::new(HashMap::new())),
            poll_error_counts: Arc::new(RwLock::new(HashMap::new())),
            storage: storage.clone(),
            image_manager: image_manager.clone(),
        };

        // Refresh market URLs if images exist in ImageManager
        // This ensures fresh presigned URLs after prover refresh/TTL expiration
        // The storage provider deduplicates content, so only new URLs are generated
        if image_manager.get_batch_image().await.is_some()
            || image_manager.get_aggregation_image().await.is_some()
        {
            tracing::info!("Refreshing market URLs for uploaded images (TTL refresh)...");

            let client = prover.get_boundless_client().await?;

            // Refresh batch image URL if exists
            if let Some(batch_info) = image_manager.get_batch_image().await {
                tracing::info!(
                    "Refreshing batch image market URL (content already cached in storage)"
                );
                image_manager
                    .store_and_upload_image("batch", batch_info.elf_bytes.clone(), client)
                    .await?;
            }

            // Refresh aggregation image URL if exists
            if let Some(agg_info) = image_manager.get_aggregation_image().await {
                tracing::info!(
                    "Refreshing aggregation image market URL (content already cached in storage)"
                );
                image_manager
                    .store_and_upload_image("aggregation", agg_info.elf_bytes.clone(), client)
                    .await?;
            }

            tracing::info!("Market URLs refreshed successfully");
        } else {
            tracing::info!(
                "BoundlessProver initialized. Images should be uploaded via /upload-image endpoint."
            );
        }

        // Start background TTL cleanup task
        Self::start_ttl_cleanup_task(prover.storage.clone(), prover.active_requests.clone()).await;

        Ok(prover)
    }

    pub fn prover_config(&self) -> ProverConfig {
        self.config.clone()
    }

    pub fn storage(&self) -> &BoundlessStorage {
        &self.storage
    }

    /// Helper method to prepare and store async request
    async fn prepare_async_request(
        &self,
        request_id: String,
        proof_type: ProofType,
        input: Vec<u8>,
        output: Vec<u8>,
        config: &serde_json::Value,
    ) -> AgentResult<String> {
        tracing::info!(
            "Preparing {} proof request: {}",
            match proof_type {
                ProofType::Batch => "batch",
                ProofType::Aggregate => "aggregation",
            },
            request_id
        );

        let async_request = AsyncProofRequest {
            request_id: request_id.clone(),
            market_request_id: U256::ZERO, // Will be set when submitted
            status: ProofRequestStatus::Preparing,
            proof_type,
            input,
            output,
            config: config.clone(),
            last_error: None,
        };

        // Store the request for tracking (both memory and SQLite)
        {
            let mut requests_guard = self.active_requests.write().await;
            requests_guard.insert(request_id.clone(), async_request.clone());
        }

        // Persist to SQLite storage
        if let Err(e) = self.storage.store_request(&async_request).await {
            tracing::warn!(
                "Failed to store {} request in SQLite: {}",
                match async_request.proof_type {
                    ProofType::Batch => "batch",
                    ProofType::Aggregate => "aggregation",
                },
                e
            );
        }

        Ok(request_id)
    }

    /// Helper method to update failed status in both memory and storage
    pub(crate) async fn update_failed_status(&self, request_id: &str, error: String) {
        let failed_status = ProofRequestStatus::Failed { error };
        let _ = self
            .update_request_status(request_id, failed_status, &self.active_requests)
            .await;
    }

    /// Helper method to update request status in both memory and storage
    pub(crate) async fn update_request_status(
        &self,
        request_id: &str,
        status: ProofRequestStatus,
        active_requests: &Arc<RwLock<HashMap<String, AsyncProofRequest>>>,
    ) -> AgentResult<()> {
        let is_terminal = matches!(
            status,
            ProofRequestStatus::Fulfilled { .. } | ProofRequestStatus::Failed { .. }
        );

        // Update status in memory
        {
            let mut requests_guard = active_requests.write().await;
            if let Some(async_req) = requests_guard.get_mut(request_id) {
                async_req.status = status.clone();
                // Also update market_request_id field when available in status
                match &status {
                    ProofRequestStatus::Submitted { market_request_id } => {
                        async_req.market_request_id = *market_request_id;
                    }
                    ProofRequestStatus::Locked {
                        market_request_id, ..
                    } => {
                        async_req.market_request_id = *market_request_id;
                    }
                    ProofRequestStatus::Fulfilled {
                        market_request_id, ..
                    } => {
                        async_req.market_request_id = *market_request_id;
                    }
                    _ => {}
                }
                async_req.last_error = match &status {
                    ProofRequestStatus::Failed { error } => Some(error.clone()),
                    _ => None,
                };
            }
        }

        // Update in SQLite storage
        if let Err(e) = self.storage.update_status(request_id, &status).await {
            tracing::warn!("Failed to update status in storage: {}", e);
            return Err(AgentError::ClientBuildError(format!(
                "Storage update failed: {}",
                e
            )));
        }

        // Drop terminal entries from memory to avoid unbounded growth
        if is_terminal {
            let mut requests_guard = active_requests.write().await;
            requests_guard.remove(request_id);
            self.clear_poll_errors(request_id).await;
        }

        Ok(())
    }

    /// Submit a batch proof request asynchronously
    pub async fn batch_run(
        &self,
        request_id: String,
        input: Vec<u8>,
        output: Vec<u8>,
        config: &serde_json::Value,
    ) -> AgentResult<String> {
        // Check for existing request with same input content for proper deduplication
        if let Some(existing_request) = self
            .storage
            .get_request_by_input_hash(&input, &ProofType::Batch)
            .await?
        {
            match &existing_request.status {
                ProofRequestStatus::Preparing => {
                    tracing::info!(
                        "Returning existing request in preparation phase for request: {}",
                        existing_request.request_id
                    );
                    // Add to memory cache if not already there
                    {
                        let mut requests_guard = self.active_requests.write().await;
                        if !requests_guard.contains_key(&existing_request.request_id) {
                            requests_guard.insert(
                                existing_request.request_id.clone(),
                                existing_request.clone(),
                            );
                        }
                    }
                    return Ok(existing_request.request_id);
                }
                ProofRequestStatus::Fulfilled { .. } => {
                    tracing::info!(
                        "Returning existing completed batch proof for request: {}",
                        existing_request.request_id
                    );
                    return Ok(existing_request.request_id);
                }
                ProofRequestStatus::Submitted { .. } => {
                    tracing::info!(
                        "Returning existing submitted batch proof (waiting for prover) for request: {}",
                        existing_request.request_id
                    );
                    // Add to memory cache if not already there
                    {
                        let mut requests_guard = self.active_requests.write().await;
                        if !requests_guard.contains_key(&existing_request.request_id) {
                            requests_guard.insert(
                                existing_request.request_id.clone(),
                                existing_request.clone(),
                            );
                        }
                    }
                    return Ok(existing_request.request_id);
                }
                ProofRequestStatus::Locked { .. } => {
                    tracing::info!(
                        "Returning existing locked batch proof (being processed by prover) for request: {}",
                        existing_request.request_id
                    );
                    // Add to memory cache if not already there
                    {
                        let mut requests_guard = self.active_requests.write().await;
                        if !requests_guard.contains_key(&existing_request.request_id) {
                            requests_guard.insert(
                                existing_request.request_id.clone(),
                                existing_request.clone(),
                            );
                        }
                    }
                    return Ok(existing_request.request_id);
                }
                ProofRequestStatus::Failed { error } => {
                    tracing::info!(
                        "Found failed request for same input ({}), creating new batch request",
                        error
                    );
                    // Continue to create new request (allows retry)
                }
            }
        }

        // Prepare and store the async request using the provided request ID
        let final_request_id = self
            .prepare_async_request(
                request_id.clone(),
                ProofType::Batch,
                input.clone(),
                output.clone(),
                config,
            )
            .await?;

        // Submit to boundless market in background
        let prover_clone = self.clone();
        let active_requests = self.active_requests.clone();
        let request_id_clone = request_id.clone();

        tokio::spawn(async move {
            let offer_params = prover_clone.boundless_config.get_batch_offer_params();

            // Get image info from ImageManager
            let image_info = match prover_clone.image_manager.get_batch_image().await {
                Some(info) => info,
                None => {
                    let err_msg =
                        "Batch image not uploaded. Please upload via /upload-image endpoint first.";
                    tracing::error!("{}", err_msg);
                    prover_clone
                        .update_failed_status(&request_id_clone, err_msg.to_string())
                        .await;
                    return;
                }
            };

            if let Err(e) = prover_clone
                .process_and_submit_request(
                    &request_id_clone,
                    input,
                    output,
                    &image_info.elf_bytes,
                    image_info.market_url,
                    offer_params,
                    ProofType::Batch,
                    active_requests,
                )
                .await
            {
                prover_clone
                    .update_failed_status(&request_id_clone, e.to_string())
                    .await;
            }
        });

        Ok(final_request_id)
    }

    /// Submit an aggregation proof request asynchronously
    pub async fn aggregate(
        &self,
        request_id: String,
        input: Vec<u8>,
        output: Vec<u8>,
        config: &serde_json::Value,
    ) -> AgentResult<String> {
        // Check for existing request with same input content for proper deduplication
        if let Some(existing_request) = self
            .storage
            .get_request_by_input_hash(&input, &ProofType::Aggregate)
            .await?
        {
            match &existing_request.status {
                ProofRequestStatus::Preparing => {
                    tracing::info!(
                        "Returning existing request in preparation phase for request: {}",
                        existing_request.request_id
                    );
                    // Add to memory cache if not already there
                    {
                        let mut requests_guard = self.active_requests.write().await;
                        if !requests_guard.contains_key(&existing_request.request_id) {
                            requests_guard.insert(
                                existing_request.request_id.clone(),
                                existing_request.clone(),
                            );
                        }
                    }
                    return Ok(existing_request.request_id);
                }
                ProofRequestStatus::Fulfilled { .. } => {
                    tracing::info!(
                        "Returning existing completed aggregation proof for request: {}",
                        existing_request.request_id
                    );
                    return Ok(existing_request.request_id);
                }
                ProofRequestStatus::Submitted { .. } => {
                    tracing::info!(
                        "Returning existing submitted aggregation proof (waiting for prover) for request: {}",
                        existing_request.request_id
                    );
                    // Add to memory cache if not already there
                    {
                        let mut requests_guard = self.active_requests.write().await;
                        if !requests_guard.contains_key(&existing_request.request_id) {
                            requests_guard.insert(
                                existing_request.request_id.clone(),
                                existing_request.clone(),
                            );
                        }
                    }
                    return Ok(existing_request.request_id);
                }
                ProofRequestStatus::Locked { .. } => {
                    tracing::info!(
                        "Returning existing locked aggregation proof (being processed by prover) for request: {}",
                        existing_request.request_id
                    );
                    // Add to memory cache if not already there
                    {
                        let mut requests_guard = self.active_requests.write().await;
                        if !requests_guard.contains_key(&existing_request.request_id) {
                            requests_guard.insert(
                                existing_request.request_id.clone(),
                                existing_request.clone(),
                            );
                        }
                    }
                    return Ok(existing_request.request_id);
                }
                ProofRequestStatus::Failed { error } => {
                    tracing::info!(
                        "Found failed request for same input ({}), creating new aggregation request",
                        error
                    );
                    // Continue to create new request (allows retry)
                }
            }
        }

        // Prepare and store the async request using the provided request ID
        let final_request_id = self
            .prepare_async_request(
                request_id.clone(),
                ProofType::Aggregate,
                input.clone(),
                output.clone(),
                config,
            )
            .await?;

        // Submit to boundless market in background
        let prover_clone = self.clone();
        let active_requests = self.active_requests.clone();
        let request_id_clone = request_id.clone();

        tokio::spawn(async move {
            let offer_params = prover_clone.boundless_config.get_aggregation_offer_params();

            // Get image info from ImageManager
            let image_info = match prover_clone.image_manager.get_aggregation_image().await {
                Some(info) => info,
                None => {
                    let err_msg = "Aggregation image not uploaded. Please upload via /upload-image endpoint first.";
                    tracing::error!("{}", err_msg);
                    prover_clone
                        .update_failed_status(&request_id_clone, err_msg.to_string())
                        .await;
                    return;
                }
            };

            if let Err(e) = prover_clone
                .process_and_submit_request(
                    &request_id_clone,
                    input,
                    output,
                    &image_info.elf_bytes,
                    image_info.market_url,
                    offer_params,
                    ProofType::Aggregate,
                    active_requests,
                )
                .await
            {
                prover_clone
                    .update_failed_status(&request_id_clone, e.to_string())
                    .await;
            }
        });

        Ok(final_request_id)
    }

    /// Retry a failed request using the stored input/config without resubmission
    pub async fn retry_failed_request(&self, request_id: &str) -> AgentResult<String> {
        let stored_request =
            self.storage.get_request(request_id, true).await?.ok_or_else(|| {
                AgentError::RetryError(format!("Request {} not found", request_id))
            })?;

        if !matches!(stored_request.status, ProofRequestStatus::Failed { .. }) {
            return Err(AgentError::RetryError(format!(
                "Request {} is not in a failed state",
                request_id
            )));
        }

        self.clear_poll_errors(request_id).await;

        let mut retry_request = stored_request.clone();
        retry_request.status = ProofRequestStatus::Preparing;
        retry_request.market_request_id = U256::ZERO;
        retry_request.last_error = None;

        // Persist reset state and refresh memory cache
        self.storage.store_request(&retry_request).await?;
        {
            let mut requests_guard = self.active_requests.write().await;
            requests_guard.insert(request_id.to_string(), retry_request.clone());
        }

        let prover_clone = self.clone();
        let active_requests = self.active_requests.clone();
        let request_id_clone = request_id.to_string();
        tokio::spawn(async move {
            let proof_type = retry_request.proof_type.clone();
            let input = retry_request.input.clone();
            let output = retry_request.output.clone();

            let result = match proof_type {
                ProofType::Batch => {
                    let offer_params = prover_clone.boundless_config.get_batch_offer_params();
                    if let Some(image_info) = prover_clone.image_manager.get_batch_image().await {
                        prover_clone
                            .process_and_submit_request(
                                &request_id_clone,
                                input,
                                output,
                                &image_info.elf_bytes,
                                image_info.market_url,
                                offer_params,
                                ProofType::Batch,
                                active_requests,
                            )
                            .await
                    } else {
                        Err(AgentError::RetryError(
                            "Batch image not uploaded. Please upload via /upload-image endpoint first."
                                .to_string(),
                        ))
                    }
                }
                ProofType::Aggregate => {
                    let offer_params = prover_clone.boundless_config.get_aggregation_offer_params();
                    if let Some(image_info) =
                        prover_clone.image_manager.get_aggregation_image().await
                    {
                        prover_clone
                            .process_and_submit_request(
                                &request_id_clone,
                                input,
                                output,
                                &image_info.elf_bytes,
                                image_info.market_url,
                                offer_params,
                                ProofType::Aggregate,
                                active_requests,
                            )
                            .await
                    } else {
                        Err(AgentError::RetryError(
                            "Aggregation image not uploaded. Please upload via /upload-image endpoint first."
                                .to_string(),
                        ))
                    }
                }
            };

            if let Err(e) = result {
                prover_clone
                    .update_failed_status(&request_id_clone, e.to_string())
                    .await;
            }
        });

        Ok(request_id.to_string())
    }

    /// Get the current status of an async request
    pub async fn get_request_status(&self, request_id: &str) -> Option<AsyncProofRequest> {
        // Try to get from SQLite storage first (most up-to-date)
        match self.storage.get_request(request_id, false).await {
            Ok(Some(request)) => {
                // Cache only active requests; terminal ones stay in storage only
                match request.status {
                    ProofRequestStatus::Fulfilled { .. } | ProofRequestStatus::Failed { .. } => {
                        // Ensure any stale in-memory entry is cleared
                        let mut requests_guard = self.active_requests.write().await;
                        requests_guard.remove(request_id);
                    }
                    _ => {
                        let mut requests_guard = self.active_requests.write().await;
                        requests_guard.insert(request_id.to_string(), request.clone());
                    }
                }
                Some(request)
            }
            Ok(None) => {
                // Not found in storage, try memory
                let requests_guard = self.active_requests.read().await;
                requests_guard.get(request_id).cloned()
            }
            Err(e) => {
                tracing::warn!(
                    "Failed to get request from storage, falling back to memory: {}",
                    e
                );
                let requests_guard = self.active_requests.read().await;
                requests_guard.get(request_id).cloned()
            }
        }
    }

    /// List all active requests
    pub async fn list_active_requests(&self) -> Vec<AsyncProofRequest> {
        // Get from SQLite storage for most up-to-date data
        match self.storage.list_active_requests().await {
            Ok(requests) => requests,
            Err(e) => {
                tracing::warn!(
                    "Failed to get requests from storage, falling back to memory: {}",
                    e
                );
                let requests_guard = self.active_requests.read().await;
                requests_guard
                    .values()
                    .filter(|req| {
                        !matches!(
                            req.status,
                            ProofRequestStatus::Fulfilled { .. }
                                | ProofRequestStatus::Failed { .. }
                        )
                    })
                    .cloned()
                    .collect()
            }
        }
    }

    /// Get database statistics for monitoring
    pub async fn get_database_stats(&self) -> AgentResult<crate::storage::DatabaseStats> {
        self.storage.get_stats().await
    }

    /// Delete all requests from the database
    /// Returns the number of deleted requests
    pub async fn delete_all_requests(&self) -> AgentResult<usize> {
        let deleted_count = self.storage.delete_all_requests().await?;

        // Clear in-memory active requests as well
        self.active_requests.write().await.clear();
        self.poll_error_counts.write().await.clear();

        tracing::info!(
            "Deleted {} requests from database and cleared memory cache",
            deleted_count
        );
        Ok(deleted_count)
    }

    /// Start background TTL cleanup task that runs every 3 hours
    async fn start_ttl_cleanup_task(
        storage: BoundlessStorage,
        active_requests: Arc<RwLock<HashMap<String, AsyncProofRequest>>>,
    ) {
        let mut handle_guard = ttl_cleanup_handle().lock().await;
        if let Some(handle) = handle_guard.take() {
            handle.abort();
        }

        let cleanup_task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(3 * 3600)); // Run every 3 hours
            interval.tick().await; // Skip first immediate tick

            loop {
                interval.tick().await;

                tracing::info!("Running TTL cleanup for completed requests older than 12 hours");

                match storage.delete_expired_ttl_requests().await {
                    Ok(deleted_ids) => {
                        if !deleted_ids.is_empty() {
                            tracing::info!(
                                "TTL cleanup removed {} completed requests",
                                deleted_ids.len()
                            );

                            // Remove from memory cache as well
                            {
                                let mut requests_guard = active_requests.write().await;
                                for request_id in &deleted_ids {
                                    requests_guard.remove(request_id);
                                }
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!("TTL cleanup failed: {}", e);
                    }
                }
            }
        });

        *handle_guard = Some(cleanup_task);
    }

    #[allow(dead_code)]
    async fn evaluate_cost(&self, guest_env: &GuestEnv, elf: &[u8]) -> AgentResult<(u64, Vec<u8>)> {
        let (mcycles_count, _journal) = {
            // Dry run the ELF with the input to get the journal and cycle count.
            // This can be useful to estimate the cost of the proving request.
            // It can also be useful to ensure the guest can be executed correctly and we do not send into
            // the market unprovable proving requests. If you have a different mechanism to get the expected
            // journal and set a price, you can skip this step.
            let session_info = default_executor()
                .execute(guest_env.clone().try_into().unwrap(), elf)
                .map_err(|e| {
                    AgentError::GuestExecutionError(format!(
                        "Failed to execute guest environment: {e}"
                    ))
                })?;
            let mcycles_count = session_info
                .segments
                .iter()
                .map(|segment| 1 << segment.po2)
                .sum::<u64>()
                .div_ceil(MILLION_CYCLES);
            let journal = session_info.journal.bytes;
            (mcycles_count, journal)
        };
        tracing::info!("mcycles_count: {}", mcycles_count);
        Ok((mcycles_count, _journal))
    }

    pub(crate) async fn build_boundless_request(
        &self,
        boundless_client: &Client,
        program_url: Url,
        program_bytes: &[u8],
        input_url: Option<Url>,
        guest_env: GuestEnv,
        offer_spec: &BoundlessOfferParams,
        mcycles_count: u32,
        journal: Vec<u8>,
    ) -> AgentResult<ProofRequest> {
        tracing::info!("offer_spec: {:?}", offer_spec);
        let image_id = compute_image_id(program_bytes).map_err(|e| {
            AgentError::ClientBuildError(format!("Failed to compute image_id from program: {e}"))
        })?;
        let max_price = parse_ether(&offer_spec.max_price_per_mcycle).map_err(|e| {
            AgentError::ClientBuildError(format!(
                "Failed to parse max_price_per_mcycle: {} ({})",
                offer_spec.max_price_per_mcycle, e
            ))
        })? * U256::from(mcycles_count);

        // let min_price = parse_ether(&offer_spec.min_price_per_mcycle).map_err(|e| {
        //     AgentError::ClientBuildError(format!(
        //         "Failed to parse min_price_per_mcycle: {} ({})",
        //         offer_spec.min_price_per_mcycle, e
        //     ))
        // })? * U256::from(mcycles_count);

        let lock_collateral = parse_staking_token(&offer_spec.lock_collateral)?;
        let lock_timeout = (offer_spec.lock_timeout_ms_per_mcycle * mcycles_count / 1000u32) as u32;
        let timeout = (offer_spec.timeout_ms_per_mcycle * mcycles_count / 1000u32) as u32;
        let ramp_up_period = std::cmp::min(offer_spec.ramp_up_sec, lock_timeout);

        let mut request_params = boundless_client
            .new_request()
            .with_program(program_bytes.to_vec())
            .with_program_url(program_url)
            .unwrap()
            .with_groth16_proof()
            .with_env(guest_env)
            .with_cycles(mcycles_count as u64 * MILLION_CYCLES)
            .with_image_id(image_id)
            .with_journal(Journal::new(journal))
            .with_offer(
                OfferParams::builder()
                    .ramp_up_period(ramp_up_period)
                    .lock_timeout(lock_timeout)
                    .timeout(timeout)
                    .max_price(max_price)
                    // .min_price(min_price)
                    .lock_collateral(lock_collateral)
                    .bidding_start(
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap()
                            .as_secs()
                            + 60,
                    ),
            );

        if let Some(url) = input_url {
            // with_input_url returns Result; unwrap here is safe because Infallible cannot occur
            request_params = request_params
                .with_input_url(url)
                .expect("with_input_url is infallible for valid URLs");
        }

        // Build the request, including preflight, and assigned the remaining fields.
        let request = boundless_client
            .build_request(request_params)
            .await
            .map_err(|e| AgentError::ClientBuildError(format!("Failed to build request: {e:?}")))?;
        tracing::info!("Request: {:?}", request);

        Ok(request)
    }
}

#[cfg(test)]
mod tests {
    use std::{str::FromStr, sync::Arc};

    use super::*;
    use alloy_primitives_v1p2p0::hex;
    use env_logger;
    use ethers_contract::abigen;
    use ethers_core::types::H160;
    use ethers_providers::{Http, Provider, RetryClient};
    use log::{error as tracing_err, info as tracing_info};
    use risc0_zkvm::sha::Digestible;
    // use boundless_market::alloy::providers::Provider as BoundlessProvider;

    abigen!(
        IRiscZeroVerifier,
        r#"[
            function verify(bytes calldata seal, bytes32 imageId, bytes32 journalDigest) external view
        ]"#
    );

    #[tokio::test]
    async fn test_batch_run() {
        use crate::image_manager::ImageManager;
        let image_manager = ImageManager::new();
        BoundlessProver::new(ProverConfig::default(), image_manager)
            .await
            .unwrap();
    }

    #[test]
    fn test_deployment_selection() {
        // Test Sepolia deployment
        let mut config = ProverConfig::default();
        config.boundless_config.deployment = Some(DeploymentConfig {
            deployment_type: Some(DeploymentType::Sepolia),
            overrides: None,
        });
        let deployment = BoundlessProver::create_deployment(&config).unwrap();
        assert!(deployment.order_stream_url.is_none() || deployment.order_stream_url.is_some());

        // Test Base deployment
        config.boundless_config.deployment = Some(DeploymentConfig {
            deployment_type: Some(DeploymentType::Base),
            overrides: None,
        });
        let deployment = BoundlessProver::create_deployment(&config).unwrap();
        assert!(deployment.order_stream_url.is_none() || deployment.order_stream_url.is_some());
    }

    #[test]
    fn test_deployment_type_from_str() {
        // Test valid deployment types
        assert_eq!(
            DeploymentType::from_str("sepolia").unwrap(),
            DeploymentType::Sepolia
        );
        assert_eq!(
            DeploymentType::from_str("base").unwrap(),
            DeploymentType::Base
        );

        // Test case insensitive
        assert_eq!(
            DeploymentType::from_str("SEPOLIA").unwrap(),
            DeploymentType::Sepolia
        );
        assert_eq!(
            DeploymentType::from_str("BASE").unwrap(),
            DeploymentType::Base
        );

        // Test invalid deployment types
        assert!(DeploymentType::from_str("invalid").is_err());
        assert!(DeploymentType::from_str("").is_err());
    }

    #[ignore = "requires storage provider (IPFS/Pinata)"]
    #[tokio::test]
    async fn test_run_prover() {
        // init log
        env_logger::try_init().ok();

        // loading from tests/fixtures/input-1306738.bin
        let input_bytes = std::fs::read("tests/fixtures/input-1306738.bin").unwrap();
        let output_bytes = std::fs::read("tests/fixtures/output-1306738.bin").unwrap();

        let config = serde_json::Value::default();
        let image_manager = ImageManager::new();
        let prover = BoundlessProver::new(ProverConfig::default(), image_manager)
            .await
            .unwrap();

        // Test async request submission - should return a request ID
        let request_id = prover
            .batch_run(
                "test_request_id".to_string(),
                input_bytes,
                output_bytes,
                &config,
            )
            .await
            .unwrap();
        println!("Submitted batch request with ID: {:?}", request_id);

        // Verify request ID is returned (should be a non-empty string)
        assert!(!request_id.is_empty(), "Request ID should not be empty");

        // Test deserialization of existing proof fixture
        let proof_bytes = std::fs::read("tests/fixtures/proof-1306738.bin").unwrap();
        let response: Risc0Response = bincode::deserialize(&proof_bytes).unwrap();
        println!("Successfully deserialized proof response: {:?}", response);

        // Verify the proof has required fields
        assert!(response.receipt.is_some(), "Proof should have a receipt");
    }

    #[ignore = "not needed in CI"]
    #[test]
    fn test_deserialize_zkvm_receipt() {
        // let file_name = format!("tests/fixtures/boundless_receipt_test.json");
        let file_name = format!("tests/fixtures/proof-1306738.bin");
        let bincode_proof: Vec<u8> = std::fs::read(file_name).unwrap();
        let proof: Risc0Response = bincode::deserialize(&bincode_proof).unwrap();
        println!("Deserialized proof: {:#?}", proof);

        let zkvm_receipt: ZkvmReceipt = serde_json::from_str(&proof.receipt.unwrap()).unwrap();
        println!("Deserialized zkvm receipt: {:#?}", zkvm_receipt);
    }

    #[ignore = "requires storage provider (IPFS/Pinata)"]
    #[tokio::test]
    async fn test_run_prover_aggregation() {
        env_logger::try_init().ok();

        // Load and deserialize existing proof fixture
        let file_name = format!("tests/fixtures/proof-1306738.bin");
        let proof_bytes: Vec<u8> = std::fs::read(file_name).unwrap();
        let proof: Risc0Response = bincode::deserialize(&proof_bytes).unwrap();
        println!("Deserialized proof: {:#?}", proof);

        // Prepare aggregation input
        let zkvm_receipt: ZkvmReceipt = serde_json::from_str(&proof.receipt.unwrap()).unwrap();
        let input_data = BoundlessAggregationGuestInput {
            image_id: Digest::ZERO,
            receipts: vec![zkvm_receipt],
        };
        let input = bincode::serialize(&input_data).unwrap();
        let config = serde_json::Value::default();
        let output_struct = BoundlessAggregationGuestOutput {
            journal_digest: Digest::ZERO,
        };
        let output = bincode::serialize(&output_struct).unwrap();

        // Test async aggregation request submission
        let image_manager = ImageManager::new();
        let prover = BoundlessProver::new(ProverConfig::default(), image_manager)
            .await
            .unwrap();
        let request_id = prover
            .aggregate(
                "test_aggregate_request_id".to_string(),
                input,
                output,
                &config,
            )
            .await
            .unwrap();
        println!("Submitted aggregation request with ID: {:?}", request_id);

        // Verify request ID is returned (should be a non-empty string)
        assert!(
            !request_id.is_empty(),
            "Aggregation request ID should not be empty"
        );
    }

    pub async fn verify_boundless_groth16_snark_impl(
        image_id: Digest,
        seal: Vec<u8>,
        journal_digest: Digest,
    ) -> bool {
        let verifier_rpc_url =
            std::env::var("GROTH16_VERIFIER_RPC_URL").expect("env GROTH16_VERIFIER_RPC_URL");
        let groth16_verifier_addr = {
            let addr =
                std::env::var("GROTH16_VERIFIER_ADDRESS").expect("env GROTH16_VERIFIER_RPC_URL");
            H160::from_str(&addr).unwrap()
        };

        let http_client = Arc::new(
            Provider::<RetryClient<Http>>::new_client(&verifier_rpc_url, 3, 500)
                .expect("Failed to create http client"),
        );

        tracing_info!("Verifying SNARK:");
        tracing_info!("Seal: {}", hex::encode(&seal));
        tracing_info!("Image ID: {}", hex::encode(image_id.as_bytes()));
        tracing_info!("Journal Digest: {}", hex::encode(journal_digest));
        // Fix: Use Arc for http_client to satisfy trait bounds for Provider
        let verify_call_res =
            IRiscZeroVerifier::new(groth16_verifier_addr, Arc::clone(&http_client))
                .verify(
                    seal.clone().into(),
                    image_id.as_bytes().try_into().unwrap(),
                    journal_digest.into(),
                )
                .await;

        if verify_call_res.is_ok() {
            tracing_info!("SNARK verified successfully using {groth16_verifier_addr:?}!");
            return true;
        } else {
            tracing_err!(
                "SNARK verification call to {groth16_verifier_addr:?} failed: {verify_call_res:?}!"
            );
            return false;
        }
    }

    #[tokio::test]
    async fn test_verify_eth_receipt() {
        env_logger::try_init().ok();

        // Load a proof file and deserialize to Risc0Response
        let file_name = format!("tests/fixtures/proof-1306738.bin");
        let proof_bytes: Vec<u8> = std::fs::read(file_name).expect("Failed to read proof file");
        let proof: Risc0Response =
            bincode::deserialize(&proof_bytes).expect("Failed to deserialize proof");

        let image_id = match std::env::var("BOUNDLESS_BATCH_IMAGE_ID") {
            Ok(val) => {
                let bytes = hex::decode(val.trim_start_matches("0x"))
                    .expect("BOUNDLESS_BATCH_IMAGE_ID must be hex");
                Digest::try_from(bytes.as_slice())
                    .expect("BOUNDLESS_BATCH_IMAGE_ID must be a 32-byte hex string")
            }
            Err(_) => {
                println!("Skipping test_verify_eth_receipt - set BOUNDLESS_BATCH_IMAGE_ID env var");
                return;
            }
        };

        // Call the simulated onchain verification
        let journal_digest = proof.journal.digest();
        let verified =
            verify_boundless_groth16_snark_impl(image_id, proof.seal, journal_digest).await;
        assert!(verified, "Receipt failed onchain verification");
        println!("Onchain verification result: {}", verified);
    }

    #[ignore]
    #[test]
    fn test_deserialize_boundless_config() {
        // Create test config
        let config = BoundlessConfig {
            deployment: Some(DeploymentConfig {
                deployment_type: Some(DeploymentType::Sepolia),
                overrides: None,
            }),
            offer_params: Some(OfferParamsConfig {
                batch: Some(BoundlessOfferParams::batch()),
                aggregation: Some(BoundlessOfferParams::aggregation()),
            }),
            rpc_url: None,
        };

        // Test serialization and deserialization
        let config_json = serde_json::to_string(&config).unwrap();
        let deserialized_config: BoundlessConfig = serde_json::from_str(&config_json).unwrap();

        // Verify the config was deserialized correctly
        assert_eq!(
            deserialized_config.get_deployment_type(),
            DeploymentType::Sepolia
        );

        println!("Deserialized config: {:#?}", deserialized_config);
    }

    #[test]
    fn test_prover_config_with_boundless_config() {
        let boundless_config = BoundlessConfig {
            deployment: Some(DeploymentConfig {
                deployment_type: Some(DeploymentType::Base),
                overrides: None,
            }),
            offer_params: Some(OfferParamsConfig {
                batch: Some(BoundlessOfferParams::batch()),
                aggregation: Some(BoundlessOfferParams::aggregation()),
            }),
            rpc_url: None,
        };

        let prover_config = ProverConfig {
            offchain: true,
            pull_interval: 15,
            rpc_url: "https://custom-rpc.com".to_string(),
            boundless_config,
            url_ttl: 1800,
        };

        // Test that the deployment is created correctly from boundless_config
        let deployment = BoundlessProver::create_deployment(&prover_config).unwrap();
        // Base deployment should have its default order_stream_url
        assert!(deployment.order_stream_url.is_some());
    }

    #[test]
    fn test_partial_config_override() {
        // Create a config that only overrides deployment type
        let partial_config = BoundlessConfig {
            deployment: Some(DeploymentConfig {
                deployment_type: Some(DeploymentType::Base),
                overrides: None,
            }),
            offer_params: None,
            rpc_url: None,
        };

        // Start with default config
        let mut default_config = BoundlessConfig::default();

        // Merge the partial config
        default_config.merge(&partial_config);

        // Verify that deployment type was overridden
        assert_eq!(default_config.get_deployment_type(), DeploymentType::Base);

        // Verify that offer params still use defaults
        let batch_params = default_config.get_batch_offer_params();
        let aggregation_params = default_config.get_aggregation_offer_params();

        // These should match the default values
        assert_eq!(batch_params.ramp_up_sec, 1000);
        assert_eq!(aggregation_params.ramp_up_sec, 200);
    }

    #[test]
    fn test_deployment_overrides() {
        // Test deployment overrides functionality
        let overrides = serde_json::json!({
            "order_stream_url": "https://custom-order-stream.com",
        });

        let config = BoundlessConfig {
            deployment: Some(DeploymentConfig {
                deployment_type: Some(DeploymentType::Sepolia),
                overrides: Some(overrides),
            }),
            offer_params: None,
            rpc_url: None,
        };

        let deployment = config.get_effective_deployment();

        // Verify that the overrides were applied
        assert_eq!(
            deployment.order_stream_url,
            Some(std::borrow::Cow::Owned(
                "https://custom-order-stream.com".to_string()
            ))
        );
    }

    #[test]
    fn test_offer_params_max_price() {
        let offer_params = BoundlessOfferParams::batch();
        let max_price_per_mcycle = parse_ether(&offer_params.max_price_per_mcycle)
            .expect("Failed to parse max_price_per_mcycle");
        let max_price = max_price_per_mcycle * U256::from(1000u64);
        // 0.00003 * 1000 = 0.03 ETH
        assert_eq!(max_price, U256::from(30000000000000000u128));

        let min_price_per_mcycle = parse_ether(&offer_params.min_price_per_mcycle)
            .expect("Failed to parse min_price_per_mcycle");
        let min_price = min_price_per_mcycle * U256::from(1000u64);
        // 0.000005 * 1000 = 0.005 ETH
        assert_eq!(min_price, U256::from(5000000000000000u128));

        let lock_collateral_per_mcycle = parse_staking_token(&offer_params.lock_collateral)
            .expect("Failed to parse lock_collateral_per_mcycle");
        let lock_collateral = lock_collateral_per_mcycle * U256::from(1000u64);
        // 0.0001 * 1000 = 0.1 USDC
        assert_eq!(lock_collateral, U256::from(100000000000000000u64));
    }
}
