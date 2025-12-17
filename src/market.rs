use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use alloy_primitives_v1p2p0::U256;
use alloy_signer_local_v1p0p12::PrivateKeySigner;
use boundless_market::{
    Client, ProofRequest,
    contracts::RequestStatus,
};
use reqwest::Url;
use risc0_ethereum_contracts_boundless::receipt::{Receipt as ContractReceipt, decode_seal};
use tokio::sync::RwLock;
use tokio::time::timeout;

use crate::config::BoundlessOfferParams;
use crate::boundless::{AgentError, AgentResult, BoundlessProver};
use crate::retry::retry_with_backoff;
use crate::types::{AsyncProofRequest, ProofRequestStatus, ProofType, Risc0Response};

const MAX_RETRY_ATTEMPTS: u32 = 5;
const DEFAULT_POLL_ERROR_THRESHOLD: u32 = 5;

impl BoundlessProver {
    pub(crate) fn poll_error_threshold(&self) -> u32 {
        std::env::var("POLL_ERROR_THRESHOLD")
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(DEFAULT_POLL_ERROR_THRESHOLD)
    }

    pub(crate) async fn record_poll_error(&self, request_id: &str) -> u32 {
        let mut guard = self.poll_error_counts.write().await;
        let entry = guard.entry(request_id.to_string()).or_insert(0);
        *entry += 1;
        *entry
    }

    pub(crate) async fn clear_poll_errors(&self, request_id: &str) {
        let mut guard = self.poll_error_counts.write().await;
        guard.remove(request_id);
    }

    /// Build a new boundless client with the current configuration.
    async fn build_boundless_client(&self) -> AgentResult<Client> {
        let deployment = Some(self.deployment.clone());
        let storage_provider = boundless_market::storage::storage_provider_from_env().ok();

        let url = Url::parse(&self.config.rpc_url)
            .map_err(|e| AgentError::ClientBuildError(format!("Invalid rpc_url: {e}")))?;
        let sender_priv_key = std::env::var("BOUNDLESS_SIGNER_KEY")
            .map_err(|_| AgentError::ClientBuildError("BOUNDLESS_SIGNER_KEY is not set".into()))?;
        let signer: PrivateKeySigner = sender_priv_key
            .parse()
            .map_err(|e| AgentError::ClientBuildError(format!("Invalid signer key: {e}")))?;

        let client = Client::builder()
            .with_rpc_url(url)
            .with_deployment(deployment)
            .with_storage_provider(storage_provider)
            .with_private_key(signer)
            .build()
            .await
            .map_err(|e| AgentError::ClientBuildError(e.to_string()))?;

        Ok(client)
    }

    /// Get a cached boundless client, initializing it once with retry.
    pub async fn get_boundless_client(&self) -> AgentResult<&Client> {
        self.client
            .get_or_try_init(|| async {
                retry_with_backoff(
                    "build_boundless_client",
                    || self.build_boundless_client(),
                    3,
                )
                .await
            })
            .await
    }

    /// Submit request to boundless market with retry logic
    pub(crate) async fn submit_request_async(
        &self,
        boundless_client: &Client,
        request: ProofRequest,
    ) -> AgentResult<U256> {
        // Send the request to the market with retry logic
        let request_id = if self.config.offchain {
            tracing::info!(
                "Submitting request offchain to {:?}",
                &self.deployment.order_stream_url
            );

            retry_with_backoff(
                "submit_request_offchain",
                || async {
                    boundless_client
                        .submit_request_offchain(&request)
                        .await
                        .map_err(|e| {
                            AgentError::RequestSubmitError(format!(
                                "Failed to submit request offchain: {e}"
                            ))
                        })
                },
                MAX_RETRY_ATTEMPTS,
            )
            .await?
            .0
        } else {
            retry_with_backoff(
                "submit_request_onchain",
                || async {
                    boundless_client
                        .submit_request_onchain(&request)
                        .await
                        .map_err(|e| {
                            AgentError::RequestSubmitError(format!(
                                "Failed to submit request onchain: {e}"
                            ))
                        })
                },
                MAX_RETRY_ATTEMPTS,
            )
            .await?
            .0
        };

        let request_id_str = format!("0x{:x}", request_id);
        tracing::info!("Request {} submitted successfully", request_id_str);

        Ok(request_id)
    }

    /// Check boundless market status and update request tracking
    pub(crate) async fn check_market_status(
        &self,
        market_request_id: U256,
        proof_type: &ProofType,
    ) -> AgentResult<ProofRequestStatus> {
        let boundless_client = self.get_boundless_client().await?;
        let request_id_str = format!("0x{:x}", market_request_id);

        // First, check the current status using get_status with retry logic
        let status_result = retry_with_backoff(
            "get_market_status",
            || async {
                boundless_client
                    .boundless_market
                    .get_status(market_request_id, Some(u64::MAX))
                    .await
                    .map_err(|e| AgentError::PollingError(e.to_string()))
            },
            3, // Fewer retries for status checks since we poll periodically
        )
        .await;

        match status_result {
            Ok(status) => {
                match status {
                    RequestStatus::Unknown => {
                        tracing::info!(
                            "Market status: MarketSubmitted({}) - open for bidding",
                            request_id_str
                        );
                        Ok(ProofRequestStatus::Submitted { market_request_id })
                    }
                    RequestStatus::Locked => {
                        tracing::info!(
                            "Market status: MarketLocked({}) - prover committed",
                            request_id_str
                        );
                        Ok(ProofRequestStatus::Locked {
                            market_request_id,
                            prover: None,
                        })
                    }
                    RequestStatus::Fulfilled => {
                        tracing::info!(
                            "Market status: MarketFulfilled({}) - proof completed",
                            request_id_str
                        );

                        // Get the actual proof data with retry logic since we know it's fulfilled
                        let fulfillment_result = retry_with_backoff(
                            "get_request_fulfillment",
                            || async {
                                boundless_client
                                    .boundless_market
                                    .get_request_fulfillment(market_request_id)
                                    .await
                                    .map_err(|e| AgentError::PollingError(e.to_string()))
                            },
                            MAX_RETRY_ATTEMPTS,
                        )
                        .await;

                        match fulfillment_result {
                            Ok(fulfillment) => {
                                // Decode fulfillment data using the data() method first
                                let journal = match fulfillment.data() {
                                    Ok(fulfillment_data) => match fulfillment_data.journal() {
                                        Some(j) => j.to_vec(),
                                        None => {
                                            tracing::error!(
                                                "No journal found in fulfillment data for {}",
                                                request_id_str
                                            );
                                            return Ok(ProofRequestStatus::Failed {
                                                error: AgentError::MissingJournalError.to_string(),
                                            });
                                        }
                                    },
                                    Err(e) => {
                                        tracing::error!(
                                            "Failed to decode fulfillment data for {}: {}",
                                            request_id_str,
                                            e
                                        );
                                        return Ok(ProofRequestStatus::Failed {
                                            error: AgentError::FulfillmentDecodeError(
                                                e.to_string(),
                                            )
                                            .to_string(),
                                        });
                                    }
                                };

                                let seal = fulfillment.seal;

                                // Decode boundless receipt only for batch proofs using the uploaded batch image ID
                                let receipt = match proof_type {
                                    ProofType::Batch => {
                                        if let Some(batch_image_id) =
                                            self.image_manager.get_batch_image_id().await
                                        {
                                            match decode_seal(
                                                seal.clone(),
                                                crate::image_manager::ImageManager::digest_to_array(
                                                    &batch_image_id,
                                                ),
                                                journal.clone(),
                                            ) {
                                                Ok(ContractReceipt::Base(boundless_receipt)) => {
                                                    serde_json::to_string(&boundless_receipt).ok()
                                                }
                                                _ => None,
                                            }
                                        } else {
                                            tracing::warn!(
                                                "Batch image ID unavailable when decoding receipt"
                                            );
                                            None
                                        }
                                    }
                                    _ => None, // Aggregation and other types get None
                                };

                                let response = Risc0Response {
                                    seal: seal.to_vec(),
                                    journal: journal.clone(),
                                    receipt,
                                };

                                let proof_bytes = bincode::serialize(&response).map_err(|e| {
                                    AgentError::ResponseEncodeError(format!(
                                        "Failed to encode response: {e}"
                                    ))
                                })?;

                                Ok(ProofRequestStatus::Fulfilled {
                                    market_request_id,
                                    proof: proof_bytes,
                                })
                            }
                            Err(e) => {
                                tracing::warn!(
                                    "Failed to get fulfillment for {}: {}",
                                    request_id_str,
                                    e
                                );
                                Err(AgentError::PollingError(format!(
                                    "Failed to get proof data: {}",
                                    e
                                )))
                            }
                        }
                    }
                    RequestStatus::Expired => {
                        tracing::warn!(
                            "Market status: MarketExpired({}) - request expired",
                            request_id_str
                        );
                        Ok(ProofRequestStatus::Failed {
                            error: "Request expired in boundless market".to_string(),
                        })
                    }
                }
            }
            Err(e) => {
                tracing::warn!("Failed to get market status for {}: {}", request_id_str, e);
                Err(AgentError::PollingError(format!(
                    "Failed to check market status: {}",
                    e
                )))
            }
        }
    }

    /// Helper method to perform a single market status poll
    pub(crate) async fn poll_market_status(
        &self,
        request_id: &str,
        market_request_id: U256,
        proof_type: &ProofType,
        active_requests: &Arc<RwLock<HashMap<String, AsyncProofRequest>>>,
    ) -> bool {
        let market_id_str = format!("0x{:x}", market_request_id);

        // Use retry logic for status polling to handle transient failures
        let status_result = retry_with_backoff(
            "check_market_status_polling",
            || self.check_market_status(market_request_id, proof_type),
            3, // Fewer retries since we poll periodically
        )
        .await;

        match status_result {
            Ok(new_status) => {
                // Reset transient poll error counter on any successful poll
                self.clear_poll_errors(request_id).await;

                // Update the status using the helper
                if let Err(e) = self
                    .update_request_status(request_id, new_status.clone(), active_requests)
                    .await
                {
                    tracing::warn!("Failed to update status for {}: {}", request_id, e);
                }

                // Check if we should stop polling (fulfilled or failed)
                match new_status {
                    ProofRequestStatus::Fulfilled { .. } => {
                        tracing::info!("Proof {} completed via market", market_id_str);
                        false // Stop polling
                    }
                    ProofRequestStatus::Failed { .. } => {
                        tracing::error!("Proof {} failed via market", market_id_str);
                        false // Stop polling
                    }
                    _ => true, // Continue polling
                }
            }
            Err(e) => {
                let count = self.record_poll_error(request_id).await;
                let threshold = self.poll_error_threshold();
                tracing::warn!(
                    "Failed to check market status for {} (attempt {} of {}): {}",
                    market_id_str,
                    count,
                    threshold,
                    e
                );

                if count >= threshold {
                    let err_msg = format!(
                        "Polling error threshold exceeded ({} consecutive errors). Last error: {}",
                        threshold, e
                    );
                    self.update_failed_status(request_id, err_msg).await;
                    self.clear_poll_errors(request_id).await;
                    false // Stop polling after exceeding threshold
                } else {
                    true // Continue polling until threshold is hit
                }
            }
        }
    }

    /// Helper method to handle polling timeout
    pub(crate) async fn handle_polling_timeout(
        &self,
        request_id: &str,
        active_requests: Arc<RwLock<HashMap<String, AsyncProofRequest>>>,
    ) {
        tracing::warn!(
            "Request {} timed out after 1 hour, marking as failed",
            request_id
        );

        let timeout_status = ProofRequestStatus::Failed {
            error: "Request timed out after 1 hour".to_string(),
        };

        // Update status using helper
        let _ = self
            .update_request_status(request_id, timeout_status, &active_requests)
            .await;

        // Remove from memory
        let mut requests_guard = active_requests.write().await;
        requests_guard.remove(request_id);

        tracing::info!("Removed timed out request {} from memory", request_id);
    }

    /// Helper method to start status polling for market requests
    pub(crate) async fn start_status_polling(
        &self,
        request_id: &str,
        market_request_id: U256,
        proof_type: ProofType,
        active_requests: Arc<RwLock<HashMap<String, AsyncProofRequest>>>,
    ) {
        let prover_clone = self.clone();
        let request_id = request_id.to_string();

        tokio::spawn(async move {
            let poll_interval = Duration::from_secs(10);

            // Create the polling future
            let pollings = async {
                while prover_clone
                    .poll_market_status(
                        &request_id,
                        market_request_id,
                        &proof_type,
                        &active_requests,
                    )
                    .await
                {
                    tokio::time::sleep(poll_interval).await;
                }
            };

            // Use timeout wrapper as suggested
            match timeout(Duration::from_secs(3600), pollings).await {
                Ok(_) => {
                    tracing::info!("Polling finished before timeout for request {}", request_id);
                }
                Err(_) => {
                    prover_clone
                        .handle_polling_timeout(&request_id, active_requests)
                        .await;
                }
            }
        });
    }

    /// Helper method to process input, build request, and submit to market
    pub(crate) async fn process_and_submit_request(
        &self,
        request_id: &str,
        input: Vec<u8>,
        output: Vec<u8>,
        elf: &[u8],
        image_url: Url,
        offer_params: BoundlessOfferParams,
        proof_type: ProofType,
        active_requests: Arc<RwLock<HashMap<String, AsyncProofRequest>>>,
    ) -> AgentResult<()> {
        let boundless_client = self.get_boundless_client().await?;

        // Process input and create guest environment
        let (guest_env, guest_env_bytes) = self.process_input(input).map_err(|e| {
            AgentError::GuestEnvEncodeError(format!("Failed to process input: {}", e))
        })?;

        // Evaluate cost
        // let (mcycles_count, _) = self.evaluate_cost(&guest_env, elf).await
        //     .map_err(|e| AgentError::GuestExecutionError(format!("Failed to evaluate cost: {}", e)))?;
        let mcycles_count = 6000;

        // Upload input to storage so provers fetch from a URL (preferred over inline)
        tracing::info!(
            "Uploading input ({} bytes) to storage provider",
            guest_env_bytes.len()
        );
        let input_url = boundless_client
            .upload_input(&guest_env_bytes)
            .await
            .map_err(|e| AgentError::UploadError(format!("Failed to upload input: {}", e)))?;
        tracing::info!("Input uploaded: {}", input_url);
        let input_url = Some(input_url);

        // Build the request
        let request = self
            .build_boundless_request(
                boundless_client,
                image_url,
                elf,
                input_url,
                guest_env,
                &offer_params,
                mcycles_count as u32,
                output,
            )
            .await
            .map_err(|e| AgentError::RequestBuildError(format!("Failed to build request: {}", e)))?;

        // Submit to market
        let market_request_id = self
            .submit_request_async(boundless_client, request)
            .await
            .map_err(|e| AgentError::RequestSubmitError(format!("Failed to submit to market: {}", e)))?;

        // Update the stored request with new market_request_id
        {
            let mut requests_guard = active_requests.write().await;
            if let Some(async_req) = requests_guard.get_mut(request_id) {
                async_req.market_request_id = market_request_id;
                async_req.status = ProofRequestStatus::Submitted { market_request_id };
            }
        }

        // Update in SQLite storage with correct market_request_id
        let submitted_status = ProofRequestStatus::Submitted { market_request_id };
        if let Err(e) = self
            .storage
            .update_status(request_id, &submitted_status)
            .await
        {
            tracing::warn!("Failed to update market request ID in storage: {}", e);
        }

        // Start polling market status in background
        self.start_status_polling(request_id, market_request_id, proof_type, active_requests)
            .await;

        Ok(())
    }
}
