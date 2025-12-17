use alloy_primitives_v1p2p0::U256;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ProofRequestStatus {
    Preparing,
    Submitted {
        market_request_id: U256,
    },
    Locked {
        market_request_id: U256,
        prover: Option<String>,
    },
    Fulfilled {
        market_request_id: U256,
        proof: Vec<u8>,
    },
    Failed {
        error: String,
    },
}

/// Async proof request tracking
#[derive(Debug, Clone, Serialize)]
pub struct AsyncProofRequest {
    pub request_id: String,
    pub market_request_id: U256,
    pub status: ProofRequestStatus,
    pub proof_type: ProofType,
    pub input: Vec<u8>,
    pub output: Vec<u8>,
    pub config: serde_json::Value,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub enum ProofType {
    Batch,
    Aggregate,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Risc0Response {
    pub seal: Vec<u8>,
    pub journal: Vec<u8>,
    pub receipt: Option<String>,
}
