// PostgreSQL event log persistence.
//
// All sequenced events are written to a single `sequenced_events` table.
// The full SequencedEvent is stored as JSONB so schema evolution is painless:
// adding new event variants or fields doesn't require ALTER TABLE.
//
// On startup the engine calls `load_all_events()` and passes the result to
// `Engine::restore_from_events()` to rebuild the in-memory state.
//
// Switching to Supabase later is a `DATABASE_URL` swap — no code changes.

use sqlx::{PgPool, Row};

use crate::events::SequencedEvent;

// ---------------------------------------------------------------------------
// PgStore
// ---------------------------------------------------------------------------

pub struct PgStore {
    pool: PgPool,
}

impl PgStore {
    /// Open a connection pool to the given Postgres URL.
    pub async fn connect(database_url: &str) -> Result<Self, sqlx::Error> {
        let pool = PgPool::connect(database_url).await?;
        Ok(Self { pool })
    }

    /// Create the `sequenced_events` table if it doesn't exist.
    ///
    /// Safe to call on every startup — idempotent via `IF NOT EXISTS`.
    pub async fn migrate(&self) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS sequenced_events (
                seq_id       BIGINT      PRIMARY KEY,
                timestamp_ms BIGINT      NOT NULL,
                event_type   TEXT        NOT NULL,
                payload      JSONB       NOT NULL,
                inserted_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
            )"#,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Persist a batch of events to Postgres in a single transaction.
    ///
    /// Uses `ON CONFLICT (seq_id) DO NOTHING` so replaying the same events
    /// is safe (e.g. after a crash mid-batch).
    pub async fn append_events(&self, events: &[SequencedEvent]) -> Result<(), sqlx::Error> {
        if events.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool.begin().await?;
        for event in events {
            let payload = serde_json::to_value(event)
                .map_err(|e| sqlx::Error::Protocol(e.to_string()))?;
            sqlx::query(
                "INSERT INTO sequenced_events (seq_id, timestamp_ms, event_type, payload)
                 VALUES ($1, $2, $3, $4)
                 ON CONFLICT (seq_id) DO NOTHING",
            )
            .bind(event.seq_id as i64)
            .bind(event.timestamp_ms as i64)
            .bind(event_type_tag(&event.event))
            .bind(payload)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Load all events from Postgres in seq_id order.
    ///
    /// Called once at startup to rebuild the in-memory engine state.
    pub async fn load_all_events(&self) -> Result<Vec<SequencedEvent>, sqlx::Error> {
        let rows =
            sqlx::query("SELECT payload FROM sequenced_events ORDER BY seq_id ASC")
                .fetch_all(&self.pool)
                .await?;

        let mut events = Vec::with_capacity(rows.len());
        for row in rows {
            let payload: serde_json::Value = row.try_get("payload")?;
            let event: SequencedEvent = serde_json::from_value(payload)
                .map_err(|e| sqlx::Error::Protocol(e.to_string()))?;
            events.push(event);
        }
        Ok(events)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn event_type_tag(event: &crate::events::Event) -> &'static str {
    use crate::events::Event;
    match event {
        Event::OrderAccepted { .. } => "order_accepted",
        Event::OrderRejected { .. } => "order_rejected",
        Event::Fill(_) => "fill",
        Event::OrderCancelled { .. } => "order_cancelled",
        Event::MarketPaused { .. } => "market_paused",
        Event::MarketResumed { .. } => "market_resumed",
    }
}
