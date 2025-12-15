use serde::{Deserialize, Serialize};

use boundless_market::deployments::{BASE, Deployment, SEPOLIA};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum DeploymentType {
    Sepolia,
    Base,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BoundlessOfferParams {
    pub ramp_up_sec: u32,
    pub lock_timeout_ms_per_mcycle: u32,
    pub timeout_ms_per_mcycle: u32,
    pub max_price_per_mcycle: String,
    pub min_price_per_mcycle: String,
    pub lock_collateral: String,
}

impl Default for BoundlessOfferParams {
    fn default() -> Self {
        Self {
            ramp_up_sec: 200,
            lock_timeout_ms_per_mcycle: 1000,
            timeout_ms_per_mcycle: 3000,
            max_price_per_mcycle: "0.00001".to_string(),
            min_price_per_mcycle: "0.000003".to_string(),
            lock_collateral: "0.0001".to_string(),
        }
    }
}

impl BoundlessOfferParams {
    pub fn aggregation() -> Self {
        Self {
            ramp_up_sec: 200,
            lock_timeout_ms_per_mcycle: 1000,
            timeout_ms_per_mcycle: 3000,
            max_price_per_mcycle: "0.00001".to_string(),
            min_price_per_mcycle: "0.000003".to_string(),
            lock_collateral: "0.0001".to_string(),
        }
    }

    pub fn batch() -> Self {
        Self {
            ramp_up_sec: 1000,
            lock_timeout_ms_per_mcycle: 5000,
            timeout_ms_per_mcycle: 3600 * 3,
            max_price_per_mcycle: "0.00003".to_string(),
            min_price_per_mcycle: "0.000005".to_string(),
            lock_collateral: "0.0001".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoundlessAggregationGuestInput {
    pub image_id: risc0_zkvm::Digest,
    pub receipts: Vec<risc0_zkvm::Receipt>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoundlessAggregationGuestOutput {
    pub journal_digest: risc0_zkvm::Digest,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BoundlessConfig {
    pub deployment: Option<DeploymentConfig>,
    pub offer_params: Option<OfferParamsConfig>,
    pub rpc_url: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeploymentConfig {
    pub deployment_type: Option<DeploymentType>,
    pub overrides: Option<serde_json::Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OfferParamsConfig {
    pub batch: Option<BoundlessOfferParams>,
    pub aggregation: Option<BoundlessOfferParams>,
}

impl Default for BoundlessConfig {
    fn default() -> Self {
        Self {
            deployment: Some(DeploymentConfig {
                deployment_type: Some(DeploymentType::Sepolia),
                overrides: None,
            }),
            offer_params: Some(OfferParamsConfig {
                batch: Some(BoundlessOfferParams::batch()),
                aggregation: Some(BoundlessOfferParams::aggregation()),
            }),
            rpc_url: None,
        }
    }
}

impl BoundlessConfig {
    /// Merge this config with another config, taking values from other where provided
    pub fn merge(&mut self, other: &BoundlessConfig) {
        // Merge deployment config if provided
        if let Some(other_deployment) = &other.deployment {
            if let Some(ref mut deployment) = self.deployment {
                // Merge deployment type
                if let Some(deployment_type) = &other_deployment.deployment_type {
                    deployment.deployment_type = Some(deployment_type.clone());
                }

                // Merge deployment overrides
                if let Some(other_overrides) = &other_deployment.overrides {
                    if let Some(ref mut overrides) = deployment.overrides {
                        // Merge JSON objects
                        if let (Some(obj1), Some(obj2)) =
                            (overrides.as_object_mut(), other_overrides.as_object())
                        {
                            for (key, value) in obj2 {
                                obj1.insert(key.clone(), value.clone());
                            }
                        }
                    } else {
                        deployment.overrides = Some(other_overrides.clone());
                    }
                }
            } else {
                self.deployment = Some(other_deployment.clone());
            }
        }

        // Merge offer params if provided
        if let Some(other_offer_params) = &other.offer_params {
            if let Some(ref mut offer_params) = self.offer_params {
                if let Some(batch) = &other_offer_params.batch {
                    offer_params.batch = Some(batch.clone());
                }
                if let Some(aggregation) = &other_offer_params.aggregation {
                    offer_params.aggregation = Some(aggregation.clone());
                }
            } else {
                self.offer_params = Some(other_offer_params.clone());
            }
        }

        // Merge rpc_url if provided
        if let Some(rpc_url) = &other.rpc_url {
            self.rpc_url = Some(rpc_url.clone());
        }
    }

    /// Get the effective deployment type, using default if not specified
    pub fn get_deployment_type(&self) -> DeploymentType {
        self.deployment
            .as_ref()
            .and_then(|d| d.deployment_type.as_ref())
            .cloned()
            .unwrap_or(DeploymentType::Sepolia)
    }

    /// Get the effective deployment configuration by merging with base deployment
    pub fn get_effective_deployment(&self) -> Deployment {
        let deployment_type = self.get_deployment_type();
        let mut deployment = match deployment_type {
            DeploymentType::Sepolia => SEPOLIA,
            DeploymentType::Base => BASE,
        };

        // Apply deployment overrides if provided
        if let Some(deployment_config) = &self.deployment {
            if let Some(overrides) = &deployment_config.overrides {
                if let Some(order_stream_url) =
                    overrides.get("order_stream_url").and_then(|v| v.as_str())
                {
                    deployment.order_stream_url =
                        Some(std::borrow::Cow::Owned(order_stream_url.to_string()));
                }
            }
        }

        deployment
    }

    /// Get the effective batch offer params, using default if not specified
    pub fn get_batch_offer_params(&self) -> BoundlessOfferParams {
        self.offer_params
            .as_ref()
            .and_then(|o| o.batch.as_ref())
            .cloned()
            .unwrap_or_else(BoundlessOfferParams::batch)
    }

    /// Get the effective aggregation offer params, using default if not specified
    pub fn get_aggregation_offer_params(&self) -> BoundlessOfferParams {
        self.offer_params
            .as_ref()
            .and_then(|o| o.aggregation.as_ref())
            .cloned()
            .unwrap_or_else(BoundlessOfferParams::aggregation)
    }
}
