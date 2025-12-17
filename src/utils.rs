use alloy_primitives_v1p2p0::{U256, hex, keccak256, utils::parse_units};

use crate::types::ProofType;
use crate::{AgentError, AgentResult};

const STAKE_TOKEN_DECIMALS: u8 = 18;

/// Generate deterministic request ID from input and proof type
pub fn generate_request_id(input: &[u8], proof_type: &ProofType) -> String {
    let input_hash = keccak256(input);
    let proof_type_str = match proof_type {
        ProofType::Batch => "batch",
        ProofType::Aggregate => "aggregate",
    };
    format!("{}_{}", hex::encode(input_hash), proof_type_str)
}

// now staking token is ZSC, so we need to parse it as ZSC whose decimals is 18
pub fn parse_staking_token(token: &str) -> AgentResult<U256> {
    let parsed = parse_units(token, STAKE_TOKEN_DECIMALS).map_err(|e| {
        AgentError::ClientBuildError(format!("Failed to parse stacking: {} ({})", token, e))
    })?;
    Ok(parsed.into())
}
