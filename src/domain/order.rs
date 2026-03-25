// Order domain types, canonical hashing, signature verification, and validation.
//
// This is the most critical module in the codebase. The canonical hash and
// signature verification defined here are the foundation of:
//   1. Replay integrity — same input always hashes to same value.
//   2. Tamper detection — any field mutation invalidates the ed25519 signature.
//
// READ BEFORE TOUCHING:
//   The `canonical_hash()` byte layout is frozen. Changing the encoding
//   invalidates all existing signed orders. If a breaking change is ever
//   needed, bump CURRENT_SCHEMA_VERSION and add a dual-parser window.

use sha2::{Digest, Sha256};
use serde::{Deserialize, Serialize};
use crate::domain::market::MarketId;

// ---------------------------------------------------------------------------
// Serde helpers for fixed-size byte arrays
//
// Serde's derive macros only auto-implement Serialize/Deserialize for arrays
// up to [u8; 32]. For [u8; 64] (ed25519 signatures) we need custom helpers.
//
// We serialize both as lowercase hex strings in JSON.
// This is human-readable, copy-pasteable, and consistent with how order_hash
// is displayed in API responses.
// ---------------------------------------------------------------------------

mod serde_hex_32 {
    use serde::{Deserializer, Serializer};
    use serde::de::Error;

    pub fn serialize<S: Serializer>(bytes: &[u8; 32], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&hex::encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 32], D::Error> {
        let s: String = serde::Deserialize::deserialize(d)?;
        let bytes = hex::decode(&s).map_err(D::Error::custom)?;
        bytes.try_into().map_err(|_| D::Error::custom("expected exactly 32 bytes"))
    }
}

mod serde_hex_64 {
    use serde::{Deserializer, Serializer};
    use serde::de::Error;

    pub fn serialize<S: Serializer>(bytes: &[u8; 64], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&hex::encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 64], D::Error> {
        let s: String = serde::Deserialize::deserialize(d)?;
        let bytes = hex::decode(&s).map_err(D::Error::custom)?;
        bytes.try_into().map_err(|_| D::Error::custom("expected exactly 64 bytes"))
    }
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Current version of the canonical order schema.
///
/// Included in the hash so that orders signed under schema v1 cannot be
/// replayed as schema v2 orders, even if all other fields are identical.
///
/// FROZEN at 1 for Phase -1. Bump only with a migration plan.
pub const CURRENT_SCHEMA_VERSION: u16 = 1;

// ---------------------------------------------------------------------------
// Enums
// ---------------------------------------------------------------------------

/// Which side of the book an order belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    /// A bid — the trader wants to buy the base asset.
    Buy,
    /// An ask — the trader wants to sell the base asset.
    Sell,
}

/// Time-in-force controls how long an unmatched order stays active.
///
/// Only GTC is in scope for Phase -1. IOC and FOK are explicitly excluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TimeInForce {
    /// Good Till Cancel: rest on the book until filled or explicitly cancelled.
    #[serde(rename = "GTC")]
    Gtc,
}

/// Lifecycle status of an order in the matching engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderStatus {
    /// Resting on the book; no quantity filled yet.
    Open,
    /// Some quantity has been filled; remainder is still resting.
    PartiallyFilled,
    /// All quantity matched. Order is removed from the book.
    Filled,
    /// Cancelled by the trader or expired on intake. Removed from the book.
    Cancelled,
    /// Cancelled by the self-trade prevention (STP) policy.
    /// The incoming taker quantity was dropped; the resting maker is untouched.
    CancelledStp,
}

// ---------------------------------------------------------------------------
// Core structs
// ---------------------------------------------------------------------------

/// The canonical signed order — exactly what the client submits to the engine.
///
/// Field layout is frozen. See `canonical_hash()` for the exact byte encoding.
///
/// Splitting concerns:
///   - `SignedOrder` = what the trader constructs and signs (client-side).
///   - `Order`       = what the engine tracks internally after acceptance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignedOrder {
    /// Schema version. Must equal CURRENT_SCHEMA_VERSION or the order is rejected.
    /// Included in the hash to prevent cross-version replay.
    pub schema_version: u16,

    /// Client-generated idempotency key — unique per trader per market.
    /// Allows safe retries: submitting the same client_order_id twice returns
    /// the original result without creating a duplicate order.
    pub client_order_id: String,

    /// The market this order targets (e.g. "BTC-USDC").
    pub market_id: MarketId,

    /// Trader identity string. In Phase -1 this is the hex-encoded ed25519
    /// public key (64 hex chars = 32 bytes). Included in the hash.
    pub trader_id: String,

    pub side: Side,

    /// Limit price in integer tick units. Must be > 0 and divisible by the
    /// market's tick_size. No floating point — ever.
    pub price_ticks: u64,

    /// Order size in integer lot units. Must be > 0 and divisible by lot_size.
    pub size_lots: u64,

    pub time_in_force: TimeInForce,

    /// Monotonically increasing counter per trader.
    /// The engine rejects any order whose nonce is not strictly greater than
    /// the last accepted nonce for this trader.
    /// Prevents replay of old signed orders even if they haven't expired.
    pub nonce: u64,

    /// Rejection deadline in epoch milliseconds.
    /// The engine rejects the order if now_ms >= expiry_ts_ms.
    /// Set to 0 to indicate "no expiry" (use carefully in production).
    pub expiry_ts_ms: u64,

    /// Client-side creation timestamp in epoch ms. Informational only.
    /// Not enforced by the engine, but included in the hash to bind the
    /// order to a specific moment from the client's perspective.
    pub created_at_ms: u64,

    /// Optional random entropy. Recommended to prevent preimage attacks
    /// (two orders with identical logical fields would otherwise have the
    /// same hash, making them indistinguishable).
    pub salt: u64,

    /// The trader's ed25519 public key (32 bytes raw, not hex).
    /// This is the key used to verify `signature`.
    /// Must match `trader_id` — the engine checks hex(trader_pubkey) == trader_id.
    /// Serialized as a 64-char hex string in JSON.
    #[serde(with = "serde_hex_32")]
    pub trader_pubkey: [u8; 32],

    /// ed25519 signature (64 bytes) over SHA-256(canonical_bytes).
    /// Excluded from the canonical hash computation.
    /// Serialized as a 128-char hex string in JSON.
    #[serde(with = "serde_hex_64")]
    pub signature: [u8; 64],
}

/// Stable, canonical SHA-256 fingerprint of a SignedOrder's content.
///
/// Used as the key for fill and cancel tracking in the state store:
///   - `filled_amount[order_hash]`
///   - `cancelled[order_hash]`
///
/// Computed once on intake. Never changes for the lifetime of an order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct OrderHash(pub [u8; 32]);

impl OrderHash {
    /// Hex-encoded representation, used in API responses and log output.
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }
}

/// Server-assigned opaque identifier for a live order in the engine.
///
/// Generated as UUIDv4 on acceptance. This is what clients receive back
/// and use for cancel-by-order-id calls. It is NOT part of the canonical
/// hash — the order_hash is the stable persistent key, not this ID.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct OrderId(pub String);

/// A live order as tracked by the matching engine.
///
/// Created from a fully validated and accepted SignedOrder.
/// The engine mutates `filled_size_lots`, `status`, and `last_update_sequence`
/// as fills and cancels occur. All other fields are immutable after creation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Order {
    /// Server-assigned unique ID. Returned to the client on acceptance.
    pub order_id: OrderId,

    /// Client-provided idempotency key.
    pub client_order_id: String,

    /// Stable canonical hash. The persistent identifier for this order.
    pub order_hash: OrderHash,

    pub market_id: MarketId,
    pub trader_id: String,
    /// Raw public key bytes — stored for signature re-verification if needed.
    /// Serialized as a 64-char hex string in JSON.
    #[serde(with = "serde_hex_32")]
    pub trader_pubkey: [u8; 32],
    pub side: Side,

    /// Limit price. Immutable after placement.
    pub price_ticks: u64,

    /// Original order size. Immutable after placement.
    pub orig_size_lots: u64,

    /// How much has been matched so far. Grows with each fill event.
    pub filled_size_lots: u64,

    pub time_in_force: TimeInForce,
    pub nonce: u64,
    pub expiry_ts_ms: u64,
    pub status: OrderStatus,

    /// Monotonic sequence number assigned at placement.
    /// This determines time priority within a price level:
    /// lower created_sequence = earlier = higher priority.
    pub created_sequence: u64,

    /// Sequence number of the last event that mutated this order's state.
    /// Used for optimistic concurrency checks and audit.
    pub last_update_sequence: u64,

    /// Wall-clock timestamp when the engine accepted this order (epoch ms).
    pub accepted_at_ms: u64,
}

impl Order {
    /// Quantity still available to be matched.
    pub fn remaining_size_lots(&self) -> u64 {
        self.orig_size_lots - self.filled_size_lots
    }

    /// True if no further matching is possible (fully filled or any cancel variant).
    pub fn is_closed(&self) -> bool {
        matches!(
            self.status,
            OrderStatus::Filled | OrderStatus::Cancelled | OrderStatus::CancelledStp
        )
    }
}

/// A single fill event — one atomic match between a taker and a maker.
///
/// Each fill:
///   1. Increases filled_size_lots for both the taker and the maker.
///   2. Updates filled_amount[order_hash] in the state store for both sides.
///   3. Is recorded as an event in the append-only event log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fill {
    pub market_id: MarketId,

    /// The aggressive order (it crossed the spread to trigger the match).
    pub taker_order_id: OrderId,
    pub taker_order_hash: OrderHash,
    pub taker_trader_id: String,

    /// The passive (resting) order that was sitting on the book.
    pub maker_order_id: OrderId,
    pub maker_order_hash: OrderHash,
    pub maker_trader_id: String,

    /// Execution price — always the MAKER's price.
    /// Taker gets price improvement; maker gets exactly what they asked for.
    pub price_ticks: u64,

    /// Quantity matched in this fill event (in lot units).
    pub size_lots: u64,

    /// Sequence number of the event that produced this fill.
    pub sequence_id: u64,

    /// Wall-clock timestamp of the fill (epoch ms).
    pub executed_at_ms: u64,
}

// ---------------------------------------------------------------------------
// impl SignedOrder
// ---------------------------------------------------------------------------

impl SignedOrder {
    /// Compute the canonical SHA-256 hash over the order fields.
    ///
    /// ──────────────────────────────────────────────────────────────────────
    /// CANONICAL BYTE LAYOUT — FROZEN AFTER PHASE -1 LAUNCH
    /// ──────────────────────────────────────────────────────────────────────
    ///
    /// Field             | Encoding
    /// ─────────────────────────────────────────────────────────────────────
    /// schema_version    | 2 bytes, big-endian u16
    /// market_id         | 2-byte length prefix (u16) + UTF-8 bytes
    /// trader_id         | 2-byte length prefix (u16) + UTF-8 bytes
    /// client_order_id   | 2-byte length prefix (u16) + UTF-8 bytes
    /// side              | 1 byte  (0 = Buy, 1 = Sell)
    /// price_ticks       | 8 bytes, big-endian u64
    /// size_lots         | 8 bytes, big-endian u64
    /// time_in_force     | 1 byte  (0 = GTC)
    /// nonce             | 8 bytes, big-endian u64
    /// expiry_ts_ms      | 8 bytes, big-endian u64
    /// created_at_ms     | 8 bytes, big-endian u64
    /// salt              | 8 bytes, big-endian u64
    /// trader_pubkey     | 32 bytes (raw, no prefix)
    /// ─────────────────────────────────────────────────────────────────────
    ///
    /// `signature` is intentionally excluded from the hash — that's the
    /// point: we hash the payload and then sign the hash.
    ///
    /// Variable-length strings use a 2-byte (u16) length prefix. Maximum
    /// string length is therefore 65535 bytes. Validate lengths before hashing.
    pub fn canonical_hash(&self) -> OrderHash {
        let mut h = Sha256::new();

        // schema_version — first, so version changes are always reflected
        h.update(self.schema_version.to_be_bytes());

        // market_id: 2-byte length + UTF-8
        let b = self.market_id.0.as_bytes();
        h.update((b.len() as u16).to_be_bytes());
        h.update(b);

        // trader_id: 2-byte length + UTF-8
        let b = self.trader_id.as_bytes();
        h.update((b.len() as u16).to_be_bytes());
        h.update(b);

        // client_order_id: 2-byte length + UTF-8
        let b = self.client_order_id.as_bytes();
        h.update((b.len() as u16).to_be_bytes());
        h.update(b);

        // side: single-byte discriminant (no ambiguity, no endianness)
        h.update([match self.side {
            Side::Buy => 0u8,
            Side::Sell => 1u8,
        }]);

        // numeric fields — all fixed-width big-endian, no alignment issues
        h.update(self.price_ticks.to_be_bytes());
        h.update(self.size_lots.to_be_bytes());

        // time_in_force: single-byte discriminant
        h.update([match self.time_in_force {
            TimeInForce::Gtc => 0u8,
        }]);

        h.update(self.nonce.to_be_bytes());
        h.update(self.expiry_ts_ms.to_be_bytes());
        h.update(self.created_at_ms.to_be_bytes());
        h.update(self.salt.to_be_bytes());

        // trader_pubkey: always exactly 32 bytes, no prefix needed
        h.update(self.trader_pubkey);

        OrderHash(h.finalize().into())
    }

    /// Verify the ed25519 signature over the canonical hash.
    ///
    /// Must be called before accepting any order into the engine.
    /// This is the primary trust boundary — if the signature passes,
    /// the engine knows the trader authorized exactly these order fields.
    ///
    /// We sign the 32-byte hash (not the raw struct bytes) to keep the
    /// signing payload compact and independent of the wire format.
    pub fn verify_signature(&self) -> Result<(), OrderError> {
        use ed25519_dalek::{Signature, Verifier, VerifyingKey};

        let hash = self.canonical_hash();

        // Parse the public key — invalid bytes means tampered or corrupt order
        let pubkey = VerifyingKey::from_bytes(&self.trader_pubkey)
            .map_err(|_| OrderError::InvalidPublicKey)?;

        let sig = Signature::from_bytes(&self.signature);

        // Verify the signature over the 32-byte hash
        pubkey
            .verify(&hash.0, &sig)
            .map_err(|_| OrderError::InvalidSignature)
    }

    /// Validate the structural fields of the order — stateless, no market config needed.
    ///
    /// This is the first pass of validation on intake. It catches malformed
    /// requests before we touch any shared state.
    ///
    /// What's checked here (pure, no external dependencies):
    ///   - schema_version == CURRENT_SCHEMA_VERSION
    ///   - market_id, trader_id, client_order_id are non-empty
    ///   - price_ticks > 0
    ///   - size_lots > 0
    ///   - order is not already expired
    ///
    /// What's NOT checked here (requires state or market config):
    ///   - tick/lot alignment             → engine checks against MarketConfig
    ///   - nonce policy                   → engine checks against StateStore
    ///   - duplicate client_order_id      → engine checks idempotency table
    ///   - balance sufficiency            → risk module checks StateStore
    ///   - signature validity             → call verify_signature() separately
    ///     (separated so callers can short-circuit validation order)
    pub fn validate_fields(&self, now_ms: u64) -> Result<(), OrderError> {
        if self.schema_version != CURRENT_SCHEMA_VERSION {
            return Err(OrderError::UnsupportedSchemaVersion(self.schema_version));
        }

        if self.market_id.0.is_empty() {
            return Err(OrderError::EmptyField("market_id"));
        }

        if self.trader_id.is_empty() {
            return Err(OrderError::EmptyField("trader_id"));
        }

        if self.client_order_id.is_empty() {
            return Err(OrderError::EmptyField("client_order_id"));
        }

        if self.price_ticks == 0 {
            return Err(OrderError::ZeroPrice);
        }

        if self.size_lots == 0 {
            return Err(OrderError::ZeroSize);
        }

        // expiry_ts_ms == 0 means "no expiry"
        // Any non-zero expiry must be strictly in the future
        if self.expiry_ts_ms != 0 && self.expiry_ts_ms <= now_ms {
            return Err(OrderError::Expired {
                expiry_ts_ms: self.expiry_ts_ms,
                now_ms,
            });
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// All errors that can occur when validating or processing an order.
///
/// These map directly to the API error codes defined in phase_-1.md section 6.
/// The variant names are intentionally verbose — they become error messages in logs.
#[derive(Debug, thiserror::Error)]
pub enum OrderError {
    #[error("unsupported schema_version {0} (engine expects {CURRENT_SCHEMA_VERSION})")]
    UnsupportedSchemaVersion(u16),

    #[error("field '{0}' must not be empty")]
    EmptyField(&'static str),

    #[error("price_ticks must be > 0")]
    ZeroPrice,

    #[error("size_lots must be > 0")]
    ZeroSize,

    #[error("order expired: expiry_ts_ms={expiry_ts_ms} <= now_ms={now_ms}")]
    Expired { expiry_ts_ms: u64, now_ms: u64 },

    #[error("trader_pubkey is not a valid ed25519 public key")]
    InvalidPublicKey,

    #[error("ed25519 signature verification failed")]
    InvalidSignature,

    #[error("price {price} not aligned to tick_size {tick_size}")]
    InvalidTick { price: u64, tick_size: u64 },

    #[error("size {size} not aligned to lot_size {lot_size}")]
    InvalidLot { size: u64, lot_size: u64 },

    #[error("nonce {nonce} is not > last accepted nonce {last_nonce} for this trader")]
    NonceInvalid { nonce: u64, last_nonce: u64 },

    #[error("client_order_id '{0}' has already been used by this trader")]
    DuplicateClientOrderId(String),

    #[error("market '{0}' not found")]
    MarketNotFound(String),

    #[error("market '{0}' is not active")]
    MarketNotActive(String),

    #[error("trader_id does not match hex(trader_pubkey)")]
    TraderIdMismatch,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal valid SignedOrder for testing validate_fields().
    /// Signature is all zeros — fine for field validation tests.
    /// Call verify_signature() separately in signature-specific tests.
    fn make_order(price: u64, size: u64, expiry: u64) -> SignedOrder {
        SignedOrder {
            schema_version: CURRENT_SCHEMA_VERSION,
            client_order_id: "client-abc-123".into(),
            market_id: MarketId("BTC-USDC".into()),
            trader_id: "deadbeef".into(),
            side: Side::Buy,
            price_ticks: price,
            size_lots: size,
            time_in_force: TimeInForce::Gtc,
            nonce: 1,
            expiry_ts_ms: expiry,
            created_at_ms: 1_000,
            salt: 42,
            trader_pubkey: [0u8; 32],
            signature: [0u8; 64],
        }
    }

    // --- validate_fields tests ---

    #[test]
    fn valid_order_passes() {
        // expiry is well in the future relative to now_ms = 1_000
        let order = make_order(50_000, 10, 99_999_999);
        assert!(order.validate_fields(1_000).is_ok());
    }

    #[test]
    fn expired_order_fails() {
        // expiry_ts_ms (500) <= now_ms (1_000) → should be rejected
        let order = make_order(50_000, 10, 500);
        assert!(matches!(
            order.validate_fields(1_000),
            Err(OrderError::Expired { .. })
        ));
    }

    #[test]
    fn zero_expiry_is_no_expiry() {
        // expiry_ts_ms == 0 means "never expires"
        let order = make_order(50_000, 10, 0);
        assert!(order.validate_fields(u64::MAX).is_ok());
    }

    #[test]
    fn zero_size_fails() {
        let order = make_order(50_000, 0, 99_999_999);
        assert!(matches!(
            order.validate_fields(1_000),
            Err(OrderError::ZeroSize)
        ));
    }

    #[test]
    fn zero_price_fails() {
        let order = make_order(0, 10, 99_999_999);
        assert!(matches!(
            order.validate_fields(1_000),
            Err(OrderError::ZeroPrice)
        ));
    }

    #[test]
    fn wrong_schema_version_fails() {
        let mut order = make_order(50_000, 10, 99_999_999);
        order.schema_version = 99;
        assert!(matches!(
            order.validate_fields(1_000),
            Err(OrderError::UnsupportedSchemaVersion(99))
        ));
    }

    #[test]
    fn empty_market_id_fails() {
        let mut order = make_order(50_000, 10, 99_999_999);
        order.market_id = MarketId("".into());
        assert!(matches!(
            order.validate_fields(1_000),
            Err(OrderError::EmptyField("market_id"))
        ));
    }

    // --- canonical_hash tests ---

    #[test]
    fn hash_is_deterministic() {
        // Same order must always produce the same hash
        let order = make_order(50_000, 10, 99_999_999);
        let h1 = order.canonical_hash();
        let h2 = order.canonical_hash();
        assert_eq!(h1, h2);
    }

    #[test]
    fn different_price_produces_different_hash() {
        let o1 = make_order(50_000, 10, 99_999_999);
        let o2 = make_order(50_001, 10, 99_999_999);
        assert_ne!(o1.canonical_hash(), o2.canonical_hash());
    }

    #[test]
    fn different_market_id_produces_different_hash() {
        let mut o1 = make_order(50_000, 10, 99_999_999);
        let mut o2 = make_order(50_000, 10, 99_999_999);
        o1.market_id = MarketId("BTC-USDC".into());
        o2.market_id = MarketId("ETH-USDC".into());
        assert_ne!(o1.canonical_hash(), o2.canonical_hash());
    }

    #[test]
    fn hash_to_hex_is_64_chars() {
        // SHA-256 = 32 bytes = 64 hex characters
        let order = make_order(50_000, 10, 99_999_999);
        assert_eq!(order.canonical_hash().to_hex().len(), 64);
    }

    // --- verify_signature tests ---
    // These tests require generating real ed25519 key pairs.

    #[test]
    fn valid_signature_verifies() {
        use ed25519_dalek::{Signer, SigningKey};
        use rand::rngs::OsRng;

        let signing_key = SigningKey::generate(&mut OsRng);
        let pubkey_bytes = signing_key.verifying_key().to_bytes();

        let mut order = make_order(50_000, 10, 99_999_999);
        order.trader_pubkey = pubkey_bytes;
        order.trader_id = hex::encode(pubkey_bytes);

        // Compute the hash, then sign it
        let hash = order.canonical_hash();
        let sig = signing_key.sign(&hash.0);
        order.signature = sig.to_bytes();

        assert!(order.verify_signature().is_ok());
    }

    #[test]
    fn tampered_order_fails_signature() {
        use ed25519_dalek::{Signer, SigningKey};
        use rand::rngs::OsRng;

        let signing_key = SigningKey::generate(&mut OsRng);
        let pubkey_bytes = signing_key.verifying_key().to_bytes();

        let mut order = make_order(50_000, 10, 99_999_999);
        order.trader_pubkey = pubkey_bytes;
        order.trader_id = hex::encode(pubkey_bytes);

        let hash = order.canonical_hash();
        let sig = signing_key.sign(&hash.0);
        order.signature = sig.to_bytes();

        // Tamper with the price after signing — signature should no longer verify
        order.price_ticks = 99_999;

        assert!(matches!(
            order.verify_signature(),
            Err(OrderError::InvalidSignature)
        ));
    }

    #[test]
    fn wrong_key_fails_signature() {
        use ed25519_dalek::{Signer, SigningKey};
        use rand::rngs::OsRng;

        let signing_key_a = SigningKey::generate(&mut OsRng);
        let signing_key_b = SigningKey::generate(&mut OsRng);
        let pubkey_b = signing_key_b.verifying_key().to_bytes();

        let mut order = make_order(50_000, 10, 99_999_999);
        // Claim pubkey B, but sign with key A
        order.trader_pubkey = pubkey_b;
        order.trader_id = hex::encode(pubkey_b);

        let hash = order.canonical_hash();
        let sig = signing_key_a.sign(&hash.0); // signed with A, not B
        order.signature = sig.to_bytes();

        assert!(matches!(
            order.verify_signature(),
            Err(OrderError::InvalidSignature)
        ));
    }
}
