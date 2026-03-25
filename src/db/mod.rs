// StateStore — the persistence model for order lifecycle state.
//
// Tracks the authoritative state for fills, cancels, nonces, and balances.
// Kept separate from the OrderBook intentionally:
//
//   The OrderBook is a runtime structure optimised for matching speed.
//   The StateStore is the persistence model optimised for auditability.
//   They stay in sync through the engine, but the StateStore can be
//   serialised independently and verified against any external system.

use std::collections::HashMap;
use sha2::{Digest, Sha256};

// ---------------------------------------------------------------------------
// StateStore
// ---------------------------------------------------------------------------

/// In-memory state that mirrors what will live on-chain.
///
/// The engine updates this atomically with each fill and cancel — no partial
/// mutations. On crash recovery, this is rebuilt from the event log via replay.
#[derive(Debug)]
pub struct StateStore {
    /// Cumulative filled quantity per order, keyed by order_hash.
    ///
    /// Invariant: filled_amounts[hash] <= order.orig_size_lots for all orders.
    /// Both the taker and maker entries are updated on every fill.
    pub filled_amounts: HashMap<[u8; 32], u64>,

    /// Whether an order has been cancelled, keyed by order_hash.
    ///
    /// Once set to true, this is permanent — cancelled orders cannot be revived.
    /// Checked by the engine on intake to reject re-submission of cancelled orders.
    pub cancelled: HashMap<[u8; 32], bool>,

    /// Highest accepted nonce per trader (keyed by trader_id = hex pubkey).
    ///
    /// Monotonic counter — any incoming order with nonce <= stored value is rejected.
    /// Starting value is 0 (no orders yet accepted from this trader).
    pub nonces: HashMap<String, u64>,

    /// Per-trader per-asset balance ledger.
    ///
    /// Key: (trader_id, asset_symbol) e.g. ("deadbeef...", "USDC")
    /// Value: available balance in integer units.
    ///
    /// Simple in-memory map; no locked/available split.
    pub balances: HashMap<(String, String), u64>,
}

impl StateStore {
    pub fn new() -> Self {
        Self {
            filled_amounts: HashMap::new(),
            cancelled: HashMap::new(),
            nonces: HashMap::new(),
            balances: HashMap::new(),
        }
    }

    // -----------------------------------------------------------------------
    // Fill accounting
    // -----------------------------------------------------------------------

    /// Record a fill: increment filled_amount for both taker and maker by qty.
    ///
    /// Both sides are updated in the same call — this is the atomic unit of
    /// fill accounting that will map directly to the on-chain settlement update.
    pub fn apply_fill(&mut self, taker_hash: [u8; 32], maker_hash: [u8; 32], qty: u64) {
        *self.filled_amounts.entry(taker_hash).or_insert(0) += qty;
        *self.filled_amounts.entry(maker_hash).or_insert(0) += qty;
    }

    /// How much of an order has been filled so far.
    pub fn filled_amount(&self, order_hash: &[u8; 32]) -> u64 {
        self.filled_amounts.get(order_hash).copied().unwrap_or(0)
    }

    // -----------------------------------------------------------------------
    // Cancel state
    // -----------------------------------------------------------------------

    /// Mark an order as cancelled. Idempotent — calling twice is safe.
    pub fn cancel_order(&mut self, order_hash: [u8; 32]) {
        self.cancelled.insert(order_hash, true);
    }

    /// Returns true if the order has been cancelled.
    pub fn is_cancelled(&self, order_hash: &[u8; 32]) -> bool {
        self.cancelled.get(order_hash).copied().unwrap_or(false)
    }

    // -----------------------------------------------------------------------
    // Nonce policy
    // -----------------------------------------------------------------------

    /// Check that the incoming nonce is strictly greater than the last accepted
    /// nonce for this trader, then update the stored max nonce.
    ///
    /// Returns Err if the nonce is invalid (equal or lower than stored max).
    /// On Ok, the stored nonce is advanced to `nonce`.
    ///
    /// Strict monotonic policy: orders must be submitted in nonce order.
    /// Standard per-account monotonic counter — prevents replay of old signed orders.
    pub fn check_and_update_nonce(
        &mut self,
        trader_id: &str,
        nonce: u64,
    ) -> Result<(), NonceError> {
        let last = self.nonces.get(trader_id).copied().unwrap_or(0);
        if nonce <= last {
            return Err(NonceError { last_nonce: last, submitted: nonce });
        }
        self.nonces.insert(trader_id.to_string(), nonce);
        Ok(())
    }

    /// The highest accepted nonce for a trader (0 if none seen yet).
    pub fn last_nonce(&self, trader_id: &str) -> u64 {
        self.nonces.get(trader_id).copied().unwrap_or(0)
    }

    // -----------------------------------------------------------------------
    // Balance ledger
    // -----------------------------------------------------------------------

    /// Credit `amount` units of `asset` to `trader_id`.
    pub fn credit(&mut self, trader_id: &str, asset: &str, amount: u64) {
        *self
            .balances
            .entry((trader_id.to_string(), asset.to_string()))
            .or_insert(0) += amount;
    }

    /// Debit `amount` units of `asset` from `trader_id`.
    ///
    /// Returns Err if the trader has insufficient balance.
    pub fn debit(
        &mut self,
        trader_id: &str,
        asset: &str,
        amount: u64,
    ) -> Result<(), InsufficientFunds> {
        let balance = self
            .balances
            .entry((trader_id.to_string(), asset.to_string()))
            .or_insert(0);
        if *balance < amount {
            return Err(InsufficientFunds { available: *balance, required: amount });
        }
        *balance -= amount;
        Ok(())
    }

    /// Current balance for a trader + asset (0 if unknown).
    pub fn balance(&self, trader_id: &str, asset: &str) -> u64 {
        self.balances
            .get(&(trader_id.to_string(), asset.to_string()))
            .copied()
            .unwrap_or(0)
    }

    // -----------------------------------------------------------------------
    // Determinism check
    // -----------------------------------------------------------------------

    /// Compute a deterministic SHA-256 fingerprint of the current state.
    ///
    /// Used to verify that two replay runs produce identical state.
    /// The hash covers filled_amounts, cancelled, and nonces.
    /// Balances are excluded — they are derived from fills and not
    /// needed for replay correctness verification.
    ///
    /// Determinism guarantee: entries are sorted before hashing so that
    /// HashMap iteration order (which is random) does not affect the result.
    pub fn checksum(&self) -> [u8; 32] {
        let mut h = Sha256::new();

        // filled_amounts: sort by key for determinism
        let mut filled: Vec<(&[u8; 32], &u64)> = self.filled_amounts.iter().collect();
        filled.sort_by_key(|(k, _)| *k);
        for (hash, qty) in filled {
            h.update(hash);
            h.update(qty.to_be_bytes());
        }

        // cancelled: sort by key
        let mut cancelled: Vec<(&[u8; 32], &bool)> = self.cancelled.iter().collect();
        cancelled.sort_by_key(|(k, _)| *k);
        for (hash, flag) in cancelled {
            h.update(hash);
            h.update([*flag as u8]);
        }

        // nonces: sort by trader_id
        let mut nonces: Vec<(&String, &u64)> = self.nonces.iter().collect();
        nonces.sort_by_key(|(k, _)| k.as_str());
        for (trader_id, nonce) in nonces {
            let b = trader_id.as_bytes();
            h.update((b.len() as u16).to_be_bytes());
            h.update(b);
            h.update(nonce.to_be_bytes());
        }

        h.finalize().into()
    }
}

// ---------------------------------------------------------------------------
// Error types
// ---------------------------------------------------------------------------

/// Nonce validation failure — the submitted nonce is not > last accepted nonce.
#[derive(Debug, thiserror::Error)]
#[error("nonce {submitted} is not > last accepted nonce {last_nonce}")]
pub struct NonceError {
    pub last_nonce: u64,
    pub submitted: u64,
}

/// Balance debit failure — the trader does not have enough funds.
#[derive(Debug, thiserror::Error)]
#[error("insufficient balance: required {required}, available {available}")]
pub struct InsufficientFunds {
    pub available: u64,
    pub required: u64,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_fill_increments_both_sides() {
        let mut s = StateStore::new();
        let taker = [1u8; 32];
        let maker = [2u8; 32];

        s.apply_fill(taker, maker, 10);
        assert_eq!(s.filled_amount(&taker), 10);
        assert_eq!(s.filled_amount(&maker), 10);

        // Second fill adds to the same counters
        s.apply_fill(taker, maker, 5);
        assert_eq!(s.filled_amount(&taker), 15);
        assert_eq!(s.filled_amount(&maker), 15);
    }

    #[test]
    fn filled_amount_defaults_to_zero() {
        let s = StateStore::new();
        assert_eq!(s.filled_amount(&[0u8; 32]), 0);
    }

    #[test]
    fn cancel_order_is_idempotent() {
        let mut s = StateStore::new();
        let hash = [3u8; 32];

        assert!(!s.is_cancelled(&hash));
        s.cancel_order(hash);
        assert!(s.is_cancelled(&hash));
        s.cancel_order(hash); // second call is safe
        assert!(s.is_cancelled(&hash));
    }

    #[test]
    fn nonce_first_order_any_positive_value_passes() {
        let mut s = StateStore::new();
        // No prior nonce — any value > 0 should pass
        assert!(s.check_and_update_nonce("trader-1", 1).is_ok());
    }

    #[test]
    fn nonce_must_be_strictly_greater() {
        let mut s = StateStore::new();
        s.check_and_update_nonce("trader-1", 5).unwrap();

        // Equal nonce rejected
        assert!(s.check_and_update_nonce("trader-1", 5).is_err());
        // Lower nonce rejected
        assert!(s.check_and_update_nonce("trader-1", 3).is_err());
        // Higher nonce accepted
        assert!(s.check_and_update_nonce("trader-1", 6).is_ok());
    }

    #[test]
    fn nonce_is_per_trader() {
        let mut s = StateStore::new();
        s.check_and_update_nonce("trader-1", 10).unwrap();

        // trader-2 has no prior nonce — starts fresh
        assert!(s.check_and_update_nonce("trader-2", 1).is_ok());
    }

    #[test]
    fn credit_and_debit_balance() {
        let mut s = StateStore::new();
        s.credit("alice", "USDC", 1000);
        assert_eq!(s.balance("alice", "USDC"), 1000);

        s.debit("alice", "USDC", 400).unwrap();
        assert_eq!(s.balance("alice", "USDC"), 600);
    }

    #[test]
    fn debit_insufficient_returns_error() {
        let mut s = StateStore::new();
        s.credit("alice", "USDC", 100);
        assert!(s.debit("alice", "USDC", 200).is_err());
        // Balance unchanged after failed debit
        assert_eq!(s.balance("alice", "USDC"), 100);
    }

    #[test]
    fn balance_defaults_to_zero() {
        let s = StateStore::new();
        assert_eq!(s.balance("unknown", "BTC"), 0);
    }

    #[test]
    fn checksum_is_deterministic() {
        let mut s = StateStore::new();
        s.apply_fill([1u8; 32], [2u8; 32], 10);
        s.cancel_order([3u8; 32]);
        s.check_and_update_nonce("trader-1", 5).unwrap();

        let c1 = s.checksum();
        let c2 = s.checksum();
        assert_eq!(c1, c2);
    }

    #[test]
    fn checksum_changes_after_state_mutation() {
        let mut s = StateStore::new();
        let before = s.checksum();

        s.apply_fill([1u8; 32], [2u8; 32], 10);
        let after = s.checksum();

        assert_ne!(before, after);
    }

    #[test]
    fn two_identical_state_paths_produce_same_checksum() {
        let mut s1 = StateStore::new();
        s1.apply_fill([1u8; 32], [2u8; 32], 10);
        s1.cancel_order([3u8; 32]);
        s1.check_and_update_nonce("trader-1", 7).unwrap();

        let mut s2 = StateStore::new();
        s2.apply_fill([1u8; 32], [2u8; 32], 10);
        s2.cancel_order([3u8; 32]);
        s2.check_and_update_nonce("trader-1", 7).unwrap();

        assert_eq!(s1.checksum(), s2.checksum());
    }
}
