// Snapshot — periodic state serialization and recovery.
//
// A snapshot captures the full engine state at a given seq_id so that on
// crash recovery we can load the snapshot and replay only events since
// that point (rather than replaying the entire log from seq_id=1).
//
// Snapshot format (JSON):
//   {
//     "schema_version": 1,
//     "snapshot_seq":   <last seq_id included>,
//     "state_checksum": "<hex>",
//     "created_at_ms":  <epoch ms>,
//     "events":         [<SequencedEvent>, ...],
//     "markets":        [<MarketConfig>, ...]
//   }
//
// The `events` array contains ALL sequenced events up to `snapshot_seq`.
// On load, replay() re-builds the OrderBook and StateStore from them.
// The `markets` array allows the engine to be fully restored without any
// external database.
//
// Phase -1 storage backend: local filesystem (one file per snapshot).
// Phase 1+: store snapshot blobs in the `snapshots` Postgres table.
//
// WHY NOT SERIALIZE THE BOOK DIRECTLY?
//   Serialising BTreeMap / VecDeque structures is fragile under schema change.
//   The event log is the canonical source of truth; a snapshot is just a
//   pre-computed replay result. Replaying from the event list is deterministic
//   and always produces the correct book, so we never need to trust a raw
//   book serialisation.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::domain::market::MarketConfig;
use crate::events::SequencedEvent;
use crate::replay::{replay, ReplayResult};

// ---------------------------------------------------------------------------
// Snapshot data structure
// ---------------------------------------------------------------------------

/// The current snapshot schema version.
///
/// Bumping this causes old snapshot files to fail deserialization cleanly
/// (via the version check in `load`) rather than silently misinterpreting data.
pub const SNAPSHOT_SCHEMA_VERSION: u16 = 1;

/// A self-contained snapshot of engine state at a given seq_id.
#[derive(Debug, Serialize, Deserialize)]
pub struct Snapshot {
    /// Schema version — must match SNAPSHOT_SCHEMA_VERSION on load.
    pub schema_version: u16,

    /// The seq_id of the last event included in this snapshot.
    /// Recovery should replay events with seq_id > snapshot_seq.
    pub snapshot_seq: u64,

    /// SHA-256 checksum of the StateStore at the time of snapshotting.
    /// Hex-encoded. Verified on load to catch corrupt snapshot files.
    pub state_checksum: String,

    /// Wall-clock time when the snapshot was created (epoch ms).
    pub created_at_ms: u64,

    /// The complete event log up to snapshot_seq.
    pub events: Vec<SequencedEvent>,

    /// All registered markets at snapshot time.
    /// Lets recovery fully restore the engine without a config file.
    pub markets: Vec<MarketConfig>,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("unsupported snapshot schema version {found} (expected {SNAPSHOT_SCHEMA_VERSION})")]
    UnsupportedVersion { found: u16 },

    #[error("checksum mismatch: stored {stored}, computed {computed}")]
    ChecksumMismatch { stored: String, computed: String },

    #[error("replay failed: {0}")]
    ReplayFailed(#[from] crate::replay::ReplayError),
}

// ---------------------------------------------------------------------------
// Snapshot creation
// ---------------------------------------------------------------------------

/// Create a snapshot from the current event log and market configs.
///
/// The caller supplies:
///   - `events`  — all sequenced events up to the desired cutoff
///   - `markets` — all registered MarketConfigs
///   - `checksum` — SHA-256 of the current StateStore (from StateStore::checksum())
///
/// The snapshot is serialised to JSON and written to `path`.
pub fn create_snapshot(
    path: &Path,
    events: &[SequencedEvent],
    markets: &[MarketConfig],
    checksum: [u8; 32],
) -> Result<Snapshot, SnapshotError> {
    let snapshot_seq = events.last().map(|e| e.seq_id).unwrap_or(0);
    let created_at_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    let snap = Snapshot {
        schema_version: SNAPSHOT_SCHEMA_VERSION,
        snapshot_seq,
        state_checksum: hex::encode(checksum),
        created_at_ms,
        events: events.to_vec(),
        markets: markets.to_vec(),
    };

    let json = serde_json::to_string_pretty(&snap)?;
    std::fs::write(path, json)?;
    Ok(snap)
}

// ---------------------------------------------------------------------------
// Snapshot loading and replay
// ---------------------------------------------------------------------------

/// Load a snapshot file and replay it to rebuild the engine state.
///
/// Returns:
///   - The `ReplayResult` (OrderBooks + StateStore rebuilt from the event log)
///   - The list of markets (so the engine can re-register them)
///   - The `snapshot_seq` (so the engine knows from where to load new events)
///
/// The checksum is verified before replay to catch corrupt files.
pub fn load_snapshot(path: &Path) -> Result<(ReplayResult, Vec<MarketConfig>, u64), SnapshotError> {
    let json = std::fs::read_to_string(path)?;
    let snap: Snapshot = serde_json::from_str(&json)?;

    if snap.schema_version != SNAPSHOT_SCHEMA_VERSION {
        return Err(SnapshotError::UnsupportedVersion { found: snap.schema_version });
    }

    // Replay the events to rebuild state.
    let result = replay(&snap.events)?;

    // Verify checksum after replay.
    let computed = hex::encode(result.state.checksum());
    if computed != snap.state_checksum {
        return Err(SnapshotError::ChecksumMismatch {
            stored: snap.state_checksum,
            computed,
        });
    }

    Ok((result, snap.markets, snap.snapshot_seq))
}

/// Find the most recent snapshot file in a directory.
///
/// Snapshot files are named `snapshot_{seq_id}.json`. This returns the path
/// with the highest seq_id number.
pub fn find_latest_snapshot(dir: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut best: Option<(u64, PathBuf)> = None;

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let stem = path.file_stem()?.to_str()?;
        // Parse "snapshot_{seq_id}" filename.
        let seq: u64 = stem.strip_prefix("snapshot_")?.parse().ok()?;
        if best.as_ref().is_none_or(|(s, _)| seq > *s) {
            best = Some((seq, path));
        }
    }

    best.map(|(_, p)| p)
}

/// Build the standard snapshot file name for a given seq_id.
pub fn snapshot_path(dir: &Path, seq_id: u64) -> PathBuf {
    dir.join(format!("snapshot_{seq_id}.json"))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::market::{MarketId, MarketStatus};
    use crate::domain::order::{Order, OrderHash, OrderId, OrderStatus, Side, TimeInForce};
    use crate::events::{Event, SequencedEvent};
    use tempfile::TempDir;

    fn btc_market() -> MarketConfig {
        MarketConfig {
            id: MarketId("BTC-USDC".into()),
            base_asset: "BTC".into(),
            quote_asset: "USDC".into(),
            tick_size: 1_000,
            lot_size: 1,
            fee_bps_maker: 10,
            fee_bps_taker: 20,
            status: MarketStatus::Active,
        }
    }

    fn accepted_event(seq: u64) -> SequencedEvent {
        let order = Order {
            order_id: OrderId(format!("o{seq}")),
            client_order_id: format!("cid-{seq}"),
            order_hash: OrderHash([seq as u8; 32]),
            market_id: MarketId("BTC-USDC".into()),
            trader_id: "trader-a".into(),
            trader_pubkey: [0u8; 32],
            side: Side::Buy,
            price_ticks: 50_000_000,
            orig_size_lots: 10,
            filled_size_lots: 0,
            time_in_force: TimeInForce::Gtc,
            nonce: seq,
            expiry_ts_ms: 0,
            status: OrderStatus::Open,
            created_sequence: seq,
            last_update_sequence: seq,
            accepted_at_ms: 1_000_000,
        };
        SequencedEvent {
            seq_id: seq,
            timestamp_ms: 1_000_000 + seq,
            event: Event::OrderAccepted { order },
        }
    }

    #[test]
    fn create_and_load_roundtrip() {
        let dir = TempDir::new().unwrap();
        let path = snapshot_path(dir.path(), 2);

        let events = vec![accepted_event(1), accepted_event(2)];
        let markets = vec![btc_market()];

        // Build real checksum by replaying first.
        let result = replay(&events).unwrap();
        let checksum = result.state.checksum();

        create_snapshot(&path, &events, &markets, checksum).unwrap();

        let (loaded, loaded_markets, snapshot_seq) = load_snapshot(&path).unwrap();
        assert_eq!(snapshot_seq, 2);
        assert_eq!(loaded_markets.len(), 1);
        assert_eq!(loaded_markets[0].id.0, "BTC-USDC");
        // Both orders still open and on the book.
        let book = &loaded.books["BTC-USDC"];
        assert_eq!(book.best_bid(), Some(50_000_000));

        // Checksum of reloaded state must match what was stored.
        assert_eq!(loaded.state.checksum(), checksum);
    }

    #[test]
    fn load_detects_checksum_mismatch() {
        let dir = TempDir::new().unwrap();
        let path = snapshot_path(dir.path(), 1);
        let events = vec![accepted_event(1)];
        let markets = vec![btc_market()];

        // Store a wrong checksum.
        let wrong_checksum = [0xffu8; 32];
        create_snapshot(&path, &events, &markets, wrong_checksum).unwrap();

        let err = load_snapshot(&path).unwrap_err();
        assert!(matches!(err, SnapshotError::ChecksumMismatch { .. }));
    }

    #[test]
    fn load_detects_unsupported_version() {
        let dir = TempDir::new().unwrap();
        let path = snapshot_path(dir.path(), 1);
        let events = vec![accepted_event(1)];
        let markets = vec![btc_market()];
        let result = replay(&events).unwrap();
        create_snapshot(&path, &events, &markets, result.state.checksum()).unwrap();

        // Overwrite the schema_version in the file.
        let mut json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        json["schema_version"] = serde_json::json!(999);
        std::fs::write(&path, serde_json::to_string(&json).unwrap()).unwrap();

        let err = load_snapshot(&path).unwrap_err();
        assert!(matches!(err, SnapshotError::UnsupportedVersion { found: 999 }));
    }

    #[test]
    fn find_latest_snapshot_returns_highest_seq() {
        let dir = TempDir::new().unwrap();
        let events: Vec<SequencedEvent> = vec![];
        let markets: Vec<MarketConfig> = vec![];
        let checksum = [0u8; 32];

        // Write snapshots for seq 10 and 20.
        create_snapshot(&snapshot_path(dir.path(), 10), &events, &markets, checksum).unwrap();
        create_snapshot(&snapshot_path(dir.path(), 20), &events, &markets, checksum).unwrap();

        let latest = find_latest_snapshot(dir.path()).unwrap();
        assert!(latest.to_str().unwrap().contains("snapshot_20"));
    }

    #[test]
    fn empty_dir_returns_none() {
        let dir = TempDir::new().unwrap();
        assert!(find_latest_snapshot(dir.path()).is_none());
    }
}
