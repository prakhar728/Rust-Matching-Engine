// Pre-trade risk checks.
//
// Phase -1 risk layer enforces three categories of limits before an order
// reaches the engine:
//
//   1. Per-trader rate limits — max order submissions per time window.
//   2. Order size limits — max single order size per market.
//   3. Price band check — reject orders too far from the last-traded price
//      (circuit breaker at the order level, before sequencing).
//
// The RiskChecker is stateful (rate-limit windows) and thread-safe
// (Arc<Mutex<RiskChecker>>). It is consulted by the REST handler BEFORE
// calling engine.process_order(), so orders that fail risk checks never
// consume a seq_id or create engine state.
//
// Design note: these checks are deliberately STATELESS with respect to the
// matching engine. They guard the gate at the API boundary; the engine itself
// only enforces cryptographic and market-rules invariants. This means risk
// limits can be updated at runtime without touching the deterministic core.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use thiserror::Error;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Per-market risk configuration.
#[derive(Debug, Clone)]
pub struct MarketRiskConfig {
    /// Maximum single order size in lots. Orders larger than this are rejected.
    pub max_order_size_lots: u64,

    /// Maximum price deviation from the reference price, expressed in basis
    /// points (1 bps = 0.01%). 0 means no band check.
    ///
    /// Example: 500 bps = 5%. If reference price is 50_000 and band is 500 bps,
    /// any order with price outside [47_500, 52_500] is rejected.
    pub price_band_bps: u64,
}

impl Default for MarketRiskConfig {
    fn default() -> Self {
        Self {
            max_order_size_lots: 1_000_000,
            price_band_bps: 0, // no band by default
        }
    }
}

/// Global risk configuration.
#[derive(Debug, Clone)]
pub struct RiskConfig {
    /// Max order submissions per trader per rolling window.
    pub max_orders_per_window: u64,

    /// Length of the rate-limit rolling window in milliseconds.
    pub window_ms: u64,

    /// Per-market overrides. If a market is not in this map, uses no size limit
    /// and no price band.
    pub markets: HashMap<String, MarketRiskConfig>,
}

impl Default for RiskConfig {
    fn default() -> Self {
        Self {
            max_orders_per_window: 100,
            window_ms: 1_000, // 100 orders per second per trader
            markets: HashMap::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum RiskError {
    #[error("rate limit exceeded: trader '{trader_id}' submitted {count} orders in {window_ms}ms (max {max})")]
    RateLimitExceeded {
        trader_id: String,
        count: u64,
        window_ms: u64,
        max: u64,
    },

    #[error("order size {size_lots} exceeds max allowed {max_lots} for market '{market_id}'")]
    OrderTooLarge {
        market_id: String,
        size_lots: u64,
        max_lots: u64,
    },

    #[error("price {price_ticks} is outside the {band_bps}bps band around reference price {reference_ticks} for market '{market_id}'")]
    PriceOutsideBand {
        market_id: String,
        price_ticks: u64,
        reference_ticks: u64,
        band_bps: u64,
    },
}

// ---------------------------------------------------------------------------
// RiskChecker
// ---------------------------------------------------------------------------

/// Rate-limit state per trader: timestamps of recent submissions within the window.
#[derive(Default)]
struct TraderWindow {
    /// Submission timestamps in epoch ms. Entries older than `window_ms` are pruned.
    timestamps: Vec<u64>,
}

/// The stateful risk checker. Wrap in `Arc<tokio::sync::Mutex<RiskChecker>>`.
pub struct RiskChecker {
    config: RiskConfig,

    /// Rolling submission windows per trader.
    windows: HashMap<String, TraderWindow>,

    /// Reference prices per market (last-traded price in ticks).
    /// Updated by the engine after each fill via `update_reference_price`.
    reference_prices: HashMap<String, u64>,

    /// Clock injection for deterministic tests.
    clock: Box<dyn Fn() -> u64 + Send + Sync>,
}

impl RiskChecker {
    pub fn new(config: RiskConfig) -> Self {
        Self::with_clock(
            config,
            Box::new(|| {
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64
            }),
        )
    }

    pub fn with_clock(config: RiskConfig, clock: Box<dyn Fn() -> u64 + Send + Sync>) -> Self {
        Self {
            config,
            windows: HashMap::new(),
            reference_prices: HashMap::new(),
            clock,
        }
    }

    /// Update the reference price for a market (call after every fill).
    pub fn update_reference_price(&mut self, market_id: &str, price_ticks: u64) {
        self.reference_prices.insert(market_id.to_string(), price_ticks);
    }

    /// Run all pre-trade checks for an incoming order.
    ///
    /// Checks are applied in order:
    ///   1. Rate limit (cheapest — purely time-based).
    ///   2. Order size (simple comparison).
    ///   3. Price band (only if reference price exists and band_bps > 0).
    ///
    /// On Ok, the rate-limit window is advanced (the submission is counted).
    /// On Err, nothing is mutated.
    pub fn check(
        &mut self,
        trader_id: &str,
        market_id: &str,
        price_ticks: u64,
        size_lots: u64,
    ) -> Result<(), RiskError> {
        let now = (self.clock)();

        // ── 1. Rate limit ────────────────────────────────────────────────
        self.check_rate_limit(trader_id, now)?;

        // ── 2. Order size ────────────────────────────────────────────────
        if let Some(market_cfg) = self.config.markets.get(market_id) {
            if size_lots > market_cfg.max_order_size_lots {
                return Err(RiskError::OrderTooLarge {
                    market_id: market_id.to_string(),
                    size_lots,
                    max_lots: market_cfg.max_order_size_lots,
                });
            }
        }

        // ── 3. Price band ────────────────────────────────────────────────
        if let Some(market_cfg) = self.config.markets.get(market_id) {
            if market_cfg.price_band_bps > 0 {
                if let Some(&reference) = self.reference_prices.get(market_id) {
                    self.check_price_band(market_id, price_ticks, reference, market_cfg.price_band_bps)?;
                }
            }
        }

        // All checks passed — commit the rate-limit entry.
        self.record_submission(trader_id, now);
        Ok(())
    }

    // ── Private helpers ──────────────────────────────────────────────────

    fn check_rate_limit(&self, trader_id: &str, now: u64) -> Result<(), RiskError> {
        let window_start = now.saturating_sub(self.config.window_ms);
        let count = self
            .windows
            .get(trader_id)
            .map(|w| w.timestamps.iter().filter(|&&t| t >= window_start).count() as u64)
            .unwrap_or(0);

        if count >= self.config.max_orders_per_window {
            return Err(RiskError::RateLimitExceeded {
                trader_id: trader_id.to_string(),
                count,
                window_ms: self.config.window_ms,
                max: self.config.max_orders_per_window,
            });
        }
        Ok(())
    }

    fn record_submission(&mut self, trader_id: &str, now: u64) {
        let window_start = now.saturating_sub(self.config.window_ms);
        let window = self.windows.entry(trader_id.to_string()).or_default();
        // Prune expired entries to keep memory bounded.
        window.timestamps.retain(|&t| t >= window_start);
        window.timestamps.push(now);
    }

    fn check_price_band(
        &self,
        market_id: &str,
        price_ticks: u64,
        reference: u64,
        band_bps: u64,
    ) -> Result<(), RiskError> {
        // band = reference * band_bps / 10_000
        let band = reference.saturating_mul(band_bps) / 10_000;
        let lo = reference.saturating_sub(band);
        let hi = reference.saturating_add(band);

        if price_ticks < lo || price_ticks > hi {
            return Err(RiskError::PriceOutsideBand {
                market_id: market_id.to_string(),
                price_ticks,
                reference_ticks: reference,
                band_bps,
            });
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with_limit(max: u64, window_ms: u64) -> RiskConfig {
        RiskConfig {
            max_orders_per_window: max,
            window_ms,
            markets: HashMap::new(),
        }
    }

    fn fixed_clock(ms: u64) -> Box<dyn Fn() -> u64 + Send + Sync> {
        Box::new(move || ms)
    }

    #[test]
    fn first_order_passes_rate_limit() {
        let mut checker = RiskChecker::with_clock(
            config_with_limit(10, 1_000),
            fixed_clock(1_000_000),
        );
        assert!(checker.check("trader-a", "BTC-USDC", 50_000, 1).is_ok());
    }

    #[test]
    fn rate_limit_blocks_at_max() {
        let mut checker = RiskChecker::with_clock(
            config_with_limit(3, 1_000),
            fixed_clock(1_000_000),
        );
        // Submit 3 orders — all should pass.
        for _ in 0..3 {
            assert!(checker.check("trader-a", "BTC-USDC", 50_000, 1).is_ok());
        }
        // 4th should fail.
        let err = checker.check("trader-a", "BTC-USDC", 50_000, 1).unwrap_err();
        assert!(matches!(err, RiskError::RateLimitExceeded { .. }));
    }

    #[test]
    fn rate_limit_is_per_trader() {
        let mut checker = RiskChecker::with_clock(
            config_with_limit(1, 1_000),
            fixed_clock(1_000_000),
        );
        assert!(checker.check("trader-a", "BTC-USDC", 50_000, 1).is_ok());
        // trader-b has a fresh window.
        assert!(checker.check("trader-b", "BTC-USDC", 50_000, 1).is_ok());
    }

    #[test]
    fn rate_limit_window_rolls_over() {
        let mut time = 1_000_000u64;
        // Use a cell to let us advance the clock.
        use std::sync::{Arc, Mutex as StdMutex};
        let clock_time = Arc::new(StdMutex::new(time));
        let ct = clock_time.clone();
        let mut checker = RiskChecker::with_clock(
            config_with_limit(2, 1_000),
            Box::new(move || *ct.lock().unwrap()),
        );

        // Submit 2 — fill the window.
        checker.check("t", "M", 1, 1).unwrap();
        checker.check("t", "M", 1, 1).unwrap();

        // 3rd fails within the window.
        assert!(checker.check("t", "M", 1, 1).is_err());

        // Advance clock by 1_001ms — window expires.
        *clock_time.lock().unwrap() = time + 1_001;

        // Now it should pass again.
        assert!(checker.check("t", "M", 1, 1).is_ok());
    }

    #[test]
    fn order_too_large_rejected() {
        let mut config = config_with_limit(100, 1_000);
        config.markets.insert("BTC-USDC".into(), MarketRiskConfig {
            max_order_size_lots: 100,
            price_band_bps: 0,
        });
        let mut checker = RiskChecker::with_clock(config, fixed_clock(1_000_000));
        let err = checker.check("trader-a", "BTC-USDC", 50_000, 101).unwrap_err();
        assert!(matches!(err, RiskError::OrderTooLarge { size_lots: 101, .. }));
    }

    #[test]
    fn order_at_max_size_passes() {
        let mut config = config_with_limit(100, 1_000);
        config.markets.insert("BTC-USDC".into(), MarketRiskConfig {
            max_order_size_lots: 100,
            price_band_bps: 0,
        });
        let mut checker = RiskChecker::with_clock(config, fixed_clock(1_000_000));
        assert!(checker.check("trader-a", "BTC-USDC", 50_000, 100).is_ok());
    }

    #[test]
    fn price_band_rejects_outside() {
        let mut config = config_with_limit(100, 1_000);
        config.markets.insert("BTC-USDC".into(), MarketRiskConfig {
            max_order_size_lots: u64::MAX,
            price_band_bps: 500, // 5%
        });
        let mut checker = RiskChecker::with_clock(config, fixed_clock(1_000_000));
        checker.update_reference_price("BTC-USDC", 50_000);

        // 50_000 * 5% = 2_500 band → [47_500, 52_500]
        // 53_000 is outside.
        let err = checker.check("trader-a", "BTC-USDC", 53_000, 1).unwrap_err();
        assert!(matches!(err, RiskError::PriceOutsideBand { price_ticks: 53_000, .. }));
    }

    #[test]
    fn price_band_passes_inside() {
        let mut config = config_with_limit(100, 1_000);
        config.markets.insert("BTC-USDC".into(), MarketRiskConfig {
            max_order_size_lots: u64::MAX,
            price_band_bps: 500,
        });
        let mut checker = RiskChecker::with_clock(config, fixed_clock(1_000_000));
        checker.update_reference_price("BTC-USDC", 50_000);
        assert!(checker.check("trader-a", "BTC-USDC", 51_000, 1).is_ok());
    }

    #[test]
    fn price_band_skipped_when_no_reference() {
        let mut config = config_with_limit(100, 1_000);
        config.markets.insert("BTC-USDC".into(), MarketRiskConfig {
            max_order_size_lots: u64::MAX,
            price_band_bps: 500,
        });
        let mut checker = RiskChecker::with_clock(config, fixed_clock(1_000_000));
        // No reference price set — band check skipped.
        assert!(checker.check("trader-a", "BTC-USDC", 999_999, 1).is_ok());
    }

    #[test]
    fn update_reference_price_enables_band() {
        let mut config = config_with_limit(100, 1_000);
        config.markets.insert("BTC-USDC".into(), MarketRiskConfig {
            max_order_size_lots: u64::MAX,
            price_band_bps: 100, // 1%
        });
        let mut checker = RiskChecker::with_clock(config, fixed_clock(1_000_000));
        // Before reference is set, any price passes.
        assert!(checker.check("trader-a", "BTC-USDC", 1, 1).is_ok());

        // Set reference.
        checker.update_reference_price("BTC-USDC", 50_000);
        // Now 1 is way outside the 1% band.
        let err = checker.check("trader-b", "BTC-USDC", 1, 1).unwrap_err();
        assert!(matches!(err, RiskError::PriceOutsideBand { .. }));
    }
}
