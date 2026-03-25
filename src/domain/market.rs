// Market domain types and validation rules.
//
// This module owns the concept of a "market" — its identity, configuration,
// and the rules for what constitutes a valid price or size within it.
//
// Why integer tick/lot sizes?
//   Floating-point arithmetic is non-deterministic across platforms and
//   compiler settings. The engine uses integer-only math so that the same
//   input always produces the exact same output — required for replay and
//   eventual on-chain verification.
//
// Design rule: this module has no knowledge of orders, the engine, or storage.
// It is purely a description of what a market is and what it allows.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Unique string identifier for a trading market.
///
/// Convention: "{BASE}-{QUOTE}", e.g. "BTC-USDC" or "ETH-USDC".
///
/// This string is included verbatim in the canonical order hash, so it must
/// be stable — renaming a market after launch would invalidate all existing
/// signed orders for that market.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MarketId(pub String);

/// Lifecycle state of a market.
///
/// State transitions:
///   Active  ──pause──>  Paused  ──resume──>  Active
///   Active  ──settle──> Settled   (terminal, no resumption)
///   Paused  ──settle──> Settled   (terminal)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MarketStatus {
    /// Normal operation: orders are accepted and matched.
    Active,
    /// Temporarily halted: no new orders accepted, existing book preserved.
    /// Triggered by admin action or a circuit breaker.
    Paused,
    /// Permanently closed. No orders or matches allowed.
    Settled,
}

/// Full configuration for a single trading market.
///
/// Stored in the `markets` DB table and loaded into memory at engine startup.
/// A market must be registered with the engine before any orders can be placed.
///
/// All prices and sizes in this engine are integers. The `tick_size` and
/// `lot_size` fields define the minimum granularity:
///
///   tick_size = 1_000 means the smallest price move is 1_000 integer units.
///   lot_size  = 1     means the smallest order size is 1 integer unit.
///
/// What those integer units represent in real terms (e.g. how many decimal
/// places) is a display concern, not an engine concern.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketConfig {
    pub id: MarketId,
    /// The asset being bought or sold (e.g. "BTC").
    pub base_asset: String,
    /// The asset used for quoting prices (e.g. "USDC").
    pub quote_asset: String,
    /// Minimum price increment. Every order price must be divisible by this.
    pub tick_size: u64,
    /// Minimum order size. Every order size must be divisible by this.
    pub lot_size: u64,
    /// Maker rebate / fee in basis points (1 bps = 0.01%).
    /// A positive value means a fee; a negative value would be a rebate (Phase -1: fee only).
    pub fee_bps_maker: u64,
    /// Taker fee in basis points. Applied to the aggressive (crossing) order.
    pub fee_bps_taker: u64,
    pub status: MarketStatus,
}

// ---------------------------------------------------------------------------
// Methods
// ---------------------------------------------------------------------------

impl MarketConfig {
    /// Returns true if the market is accepting new orders.
    pub fn is_active(&self) -> bool {
        self.status == MarketStatus::Active
    }

    /// Check that a price is positive and aligned to tick_size.
    ///
    /// Called by the engine on every incoming order before matching.
    /// Alignment is enforced so that price levels in the book are always
    /// on valid ticks — this keeps the book structure predictable.
    pub fn validate_price(&self, price_ticks: u64) -> Result<(), MarketError> {
        if price_ticks == 0 {
            return Err(MarketError::ZeroPrice);
        }
        if !price_ticks.is_multiple_of(self.tick_size) {
            return Err(MarketError::InvalidTick {
                price: price_ticks,
                tick_size: self.tick_size,
            });
        }
        Ok(())
    }

    /// Check that a size is positive and aligned to lot_size.
    ///
    /// Same rationale as validate_price — prevents fractional lots from
    /// causing fill accounting edge cases.
    pub fn validate_size(&self, size_lots: u64) -> Result<(), MarketError> {
        if size_lots == 0 {
            return Err(MarketError::ZeroSize);
        }
        if !size_lots.is_multiple_of(self.lot_size) {
            return Err(MarketError::InvalidLot {
                size: size_lots,
                lot_size: self.lot_size,
            });
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors that arise from market-level validation.
#[derive(Debug, thiserror::Error)]
pub enum MarketError {
    #[error("price must be positive (got 0)")]
    ZeroPrice,

    #[error("size must be positive (got 0)")]
    ZeroSize,

    #[error("price {price} is not aligned to tick_size {tick_size} (price % tick_size != 0)")]
    InvalidTick { price: u64, tick_size: u64 },

    #[error("size {size} is not aligned to lot_size {lot_size} (size % lot_size != 0)")]
    InvalidLot { size: u64, lot_size: u64 },

    #[error("market is not active")]
    NotActive,

    #[error("market not found: {0}")]
    NotFound(String),
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: build a minimal active market config for testing.
    fn test_market() -> MarketConfig {
        MarketConfig {
            id: MarketId("BTC-USDC".into()),
            base_asset: "BTC".into(),
            quote_asset: "USDC".into(),
            tick_size: 1_000,   // price must be a multiple of 1_000
            lot_size: 1,        // size must be a multiple of 1
            fee_bps_maker: 10,  // 0.10% maker fee
            fee_bps_taker: 20,  // 0.20% taker fee
            status: MarketStatus::Active,
        }
    }

    #[test]
    fn valid_price_passes() {
        let m = test_market();
        // 50_000_000 is divisible by 1_000, so it's valid
        assert!(m.validate_price(50_000_000).is_ok());
    }

    #[test]
    fn zero_price_fails() {
        let m = test_market();
        assert!(matches!(m.validate_price(0), Err(MarketError::ZeroPrice)));
    }

    #[test]
    fn unaligned_price_fails() {
        let m = test_market();
        // 50_000_500 % 1_000 == 500, not aligned
        assert!(matches!(
            m.validate_price(50_000_500),
            Err(MarketError::InvalidTick { .. })
        ));
    }

    #[test]
    fn valid_size_passes() {
        let m = test_market();
        assert!(m.validate_size(10).is_ok());
    }

    #[test]
    fn zero_size_fails() {
        let m = test_market();
        assert!(matches!(m.validate_size(0), Err(MarketError::ZeroSize)));
    }

    #[test]
    fn paused_market_is_not_active() {
        let mut m = test_market();
        m.status = MarketStatus::Paused;
        assert!(!m.is_active());
    }
}
