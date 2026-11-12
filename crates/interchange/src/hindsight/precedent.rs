//! Slice A A0: Hindsight wire precedent (frozen from omp, 2026-11-12).
//! Source: omp packages/coding-agent/src/hindsight/client.ts
//! (hand-rolled fetch; no SDK) plus settings.ts (env defaults).
//! Retain: POST /v1/default/banks/{bank}/memories with items of
//! content, timestamp, context, metadata, document_id, tags,
//! observation_scopes, strategy, update_mode, plus async flag (60s).
//! Recall: POST .../memories/recall with query, types, max_tokens,
//! budget (default mid), tags, tags_match (30s).
//! Reflect: POST /v1/default/banks/{bank}/reflect with query, context,
//! budget (default low), tags, tags_match (120s).
//! Auth: optional Bearer apiKey. Base URL default http://localhost:8888
//! (HINDSIGHT_API_URL), token HINDSIGHT_API_TOKEN optional.
//! Divergences are plan amendments, not silent drift.

/// Precedent timeouts (ms): request default, retain, recall, reflect.
pub const PRECEDENT_TIMEOUTS_MS: (u64, u64, u64, u64) = (30_000, 60_000, 30_000, 120_000);
/// Precedent default base URL (localhost, no auth).
pub const PRECEDENT_BASE_URL: &str = "http://localhost:8888";
