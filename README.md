# RISC0 Boundless Agent

Axum-based web service that brokers Batch and Aggregation proof requests to Boundless Market. The agent persists requests in SQLite, retries transient market errors with backoff, and polls fulfillment until completion.

## What it does
- Accepts async proof jobs via HTTP (`/proof`) for `Batch` and `Aggregate` types.
- Uploads guest ELFs to Boundless Market via `/upload-image/{batch|aggregation}` and keeps URLs fresh for TTL.
- Stores and tracks requests in SQLite with background cleanup and retry support (`/retry/{request_id}`).
- Polls market status with transient-error tolerance; only terminal failures mark a request as failed.
- Serves OpenAPI/Swagger UI for all endpoints.

## Build & run
```bash
cargo build --release

# Minimal run (make sure envs below are set)
BOUNDLESS_SIGNER_KEY=0x... ./target/release/boundless-agent \
  --address 0.0.0.0 --port 9999 \
  --rpc-url https://base-rpc.publicnode.com \
  --pull-interval 10 \
  --url-ttl 1800 \
  --config-file ./config/boundless.example.json
```

Key CLI flags (see `--help`):
- `--address` / `--port`: HTTP bind.
- `--rpc-url`: Market RPC URL (overrides config file).
- `--pull-interval`: Poll interval seconds for market status (min 5s).
- `--url-ttl`: How long to keep presigned URLs before re-upload.
- `--offchain`: Submit requests offchain instead of onchain.
- `--config-file`: JSON config merged over defaults (deployment/offer params/RPC URL).

## Required environment
- `BOUNDLESS_SIGNER_KEY`: Hex private key used for market submissions.
- `BOUNDLESS_RPC_URL`: Optional; defaults to Base public RPC unless overridden by config/flag.
- `SQLITE_DB_PATH`: Optional; path to request DB (default `./boundless_requests.db`).
- `POLL_ERROR_THRESHOLD`: Optional; consecutive poll failures before marking failed (default 5).

## Storage & cleanup
- Requests are persisted in SQLite; in-memory cache holds only non-terminal requests.
- Terminal requests (`fulfilled`/`failed`) are deleted after ~3 hours (set at insert time) by a background cleanup task running every 3 hours.
- SQLite is configured for incremental auto-vacuum, and TTL cleanup triggers `wal_checkpoint(TRUNCATE)` + `incremental_vacuum(0)` after deletions to reclaim disk space.

## API (HTTP)
- `POST /proof`: Submit async proof. Body:
  ```json
  { "input": [..bytes..], "output": [..bytes..], "proof_type": "Batch|Aggregate", "config": {} }
  ```
  Returns `{ request_id, market_request_id, status }` (status starts at `preparing`).
- `GET /status/{request_id}`: Detailed status + proof bytes when complete.
- `GET /requests`: List active requests.
- `POST /retry/{request_id}`: Manually retry a failed request using stored input/config.
- `DELETE /requests`: Delete all stored requests.
- `POST /upload-image/{batch|aggregation}`: Upload ELF; agent stores and posts to market.
- `GET /images`: Returns uploaded image IDs/URLs.
- `GET /health`: Liveness.
- Swagger UI: `/swagger-ui` or `/scalar`.

Only Batch/Aggregate are supported; the Update path has been removed.

## Configuration
`config/boundless.example.json` shows the shape. Core pieces:
- `deployment.deployment_type`: `Sepolia` or `Base` (merged with defaults).
- `deployment.overrides.order_stream_url`: Optional URL override.
- `offer_params.batch|aggregation`: Pricing/timeouts for each proof type (see `src/config.rs` defaults).
- `rpc_url`: Optional RPC override.

At startup the agent merges defaults with the provided config file and CLI flags.

## Workflow tips
1) Upload both ELFs via `/upload-image/batch` and `/upload-image/aggregation` before sending proofs.
2) Submit proof via `/proof` with `proof_type` `Batch` or `Aggregate`.
3) Poll `/status/{id}` or rely on `/requests` for progress; errors are retriable via `/retry/{id}`.

## Development
```bash
cargo check
cargo test
```

Modules are split for clarity: `main.rs` wiring, `api/` HTTP handlers/DTOs, `boundless.rs` orchestration, `market.rs` market I/O + polling, `storage.rs` SQLite, `image_manager.rs` ELF lifecycle, plus `config.rs`/`types.rs`/`utils.rs`/`retry.rs`.
RUST_LOG=debug cargo test
``` 
