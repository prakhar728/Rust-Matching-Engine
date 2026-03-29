// clob — CLI for interacting with the CLOB matching engine.
//
// Usage: clob [--server <url>] <subcommand>
//
// The server URL defaults to http://localhost:8080 and can be overridden
// with the CLOB_SERVER environment variable.
//
// Subcommands:
//   keygen                    — generate a new ed25519 keypair
//   markets                   — list all registered markets
//   book   <market_id>        — L2 depth snapshot
//   trades <market_id>        — recent fills
//   order  <order_id>         — get order status
//   place  ...                — sign and submit a limit order
//   cancel <order_id>         — cancel by server order ID
//   cancel-by-client-id ...   — cancel by client order ID
//   admin pause    <market>   — pause a market (requires admin key)
//   admin resume   <market>   — resume a market (requires admin key)
//   admin cancel-all <market> — cancel all open orders (requires admin key)

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::{Parser, Subcommand};
use ed25519_dalek::{Signer, SigningKey};
use rand::rngs::OsRng;

use clob_api::domain::market::MarketId;
use clob_api::domain::order::{Side, SignedOrder, TimeInForce, CURRENT_SCHEMA_VERSION};

// ---------------------------------------------------------------------------
// CLI structure
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(name = "clob", about = "CLOB matching engine CLI", version)]
struct Cli {
    /// Base URL of the running CLOB server.
    #[arg(long, default_value = "http://localhost:8080", env = "CLOB_SERVER")]
    server: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Generate a new ed25519 keypair and write the signing key to a file.
    ///
    /// The file contains 64 hex characters (the 32-byte signing key seed).
    /// Keep it secret — it is the only credential needed to place and cancel
    /// orders under that trader identity.
    ///
    /// The corresponding trader_id (hex public key) is printed to stdout.
    Keygen {
        /// Output path for the signing key file.
        #[arg(long, short, default_value = "trader.key")]
        output: PathBuf,
    },

    /// List all registered markets and their configuration.
    Markets,

    /// Show the L2 order book (aggregated by price level) for a market.
    Book {
        market_id: String,

        /// Number of price levels to show on each side.
        #[arg(long, default_value = "10")]
        depth: u32,
    },

    /// Show recent fills for a market.
    Trades {
        market_id: String,

        /// Maximum number of fills to return.
        #[arg(long, default_value = "20")]
        limit: u32,

        /// Only return fills with seq_id >= this value.
        #[arg(long)]
        from_seq: Option<u64>,
    },

    /// Get the current status of an order by its server-assigned ID.
    Order {
        order_id: String,
    },

    /// Sign and submit a limit order.
    ///
    /// Loads the signing key from --key, constructs a SignedOrder, signs it
    /// with ed25519, and POSTs it to the server.
    Place {
        /// Market to trade (e.g. BTC-USDC).
        #[arg(long)]
        market: String,

        /// Order side: "buy" or "sell".
        #[arg(long)]
        side: String,

        /// Limit price in tick units (integer, no decimals).
        #[arg(long)]
        price: u64,

        /// Order size in lot units (integer, no decimals).
        #[arg(long)]
        size: u64,

        /// Path to the signing key file (produced by `clob keygen`).
        #[arg(long)]
        key: PathBuf,

        /// Client order ID for idempotency. Auto-generated UUID if omitted.
        #[arg(long)]
        client_id: Option<String>,

        /// Nonce (must be strictly increasing per trader).
        /// Defaults to the current Unix timestamp in milliseconds.
        #[arg(long)]
        nonce: Option<u64>,
    },

    /// Cancel an order by its server-assigned order ID.
    ///
    /// Authenticates via the X-Trader-Id header (your hex public key).
    Cancel {
        order_id: String,

        /// Path to the signing key file — used to derive your trader_id.
        #[arg(long)]
        key: PathBuf,
    },

    /// Cancel an order by your client-assigned order ID.
    CancelByClientId {
        /// The client_order_id you provided when placing the order.
        #[arg(long)]
        client_id: String,

        /// Path to the signing key file — used to derive your trader_id.
        #[arg(long)]
        key: PathBuf,
    },

    /// Admin operations — require X-Admin-Key authentication.
    Admin {
        #[command(subcommand)]
        command: AdminCommand,
    },
}

#[derive(Subcommand)]
enum AdminCommand {
    /// Pause a market (stops accepting new orders).
    Pause {
        market_id: String,

        /// Admin key (reads CLOB_ADMIN_KEY env var if not provided).
        #[arg(long, env = "CLOB_ADMIN_KEY")]
        admin_key: String,

        #[arg(long, default_value = "manual")]
        reason: String,
    },

    /// Resume a paused market.
    Resume {
        market_id: String,

        #[arg(long, env = "CLOB_ADMIN_KEY")]
        admin_key: String,

        #[arg(long, default_value = "manual")]
        reason: String,
    },

    /// Cancel all open orders in a market.
    CancelAll {
        market_id: String,

        #[arg(long, env = "CLOB_ADMIN_KEY")]
        admin_key: String,
    },
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let client = reqwest::Client::new();
    let server = cli.server.trim_end_matches('/').to_string();

    let result = run(&client, &server, cli.command).await;
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

async fn run(
    client: &reqwest::Client,
    server: &str,
    command: Command,
) -> Result<(), Box<dyn std::error::Error>> {
    match command {
        Command::Keygen { output } => cmd_keygen(&output),

        Command::Markets => {
            let resp: serde_json::Value =
                client.get(format!("{server}/v1/markets")).send().await?.json().await?;
            print_json(&resp);
            Ok(())
        }

        Command::Book { market_id, depth } => {
            let resp: serde_json::Value = client
                .get(format!("{server}/v1/books/{market_id}"))
                .query(&[("depth", depth)])
                .send()
                .await?
                .json()
                .await?;
            print_book(&resp);
            Ok(())
        }

        Command::Trades { market_id, limit, from_seq } => {
            let mut query = vec![("limit", limit.to_string())];
            if let Some(seq) = from_seq {
                query.push(("from_seq", seq.to_string()));
            }
            let resp: serde_json::Value = client
                .get(format!("{server}/v1/trades/{market_id}"))
                .query(&query)
                .send()
                .await?
                .json()
                .await?;
            print_json(&resp);
            Ok(())
        }

        Command::Order { order_id } => {
            let resp: serde_json::Value = client
                .get(format!("{server}/v1/orders/{order_id}"))
                .send()
                .await?
                .json()
                .await?;
            print_json(&resp);
            Ok(())
        }

        Command::Place { market, side, price, size, key, client_id, nonce } => {
            cmd_place(client, server, market, side, price, size, key, client_id, nonce).await
        }

        Command::Cancel { order_id, key } => {
            let signing_key = load_key(&key)?;
            let trader_id = hex::encode(signing_key.verifying_key().to_bytes());

            let resp = client
                .delete(format!("{server}/v1/orders/{order_id}"))
                .header("X-Trader-Id", &trader_id)
                .send()
                .await?;

            let body: serde_json::Value = resp.json().await?;
            print_json(&body);
            Ok(())
        }

        Command::CancelByClientId { client_id, key } => {
            let signing_key = load_key(&key)?;
            let trader_id = hex::encode(signing_key.verifying_key().to_bytes());

            let body = serde_json::json!({
                "trader_id": trader_id,
                "client_order_id": client_id,
            });

            let resp: serde_json::Value = client
                .post(format!("{server}/v1/orders/cancel-by-client-id"))
                .json(&body)
                .send()
                .await?
                .json()
                .await?;
            print_json(&resp);
            Ok(())
        }

        Command::Admin { command } => cmd_admin(client, server, command).await,
    }
}

// ---------------------------------------------------------------------------
// Command implementations
// ---------------------------------------------------------------------------

fn cmd_keygen(output: &PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    let signing_key = SigningKey::generate(&mut OsRng);
    let pubkey = signing_key.verifying_key();

    // Store the 32-byte signing key seed as hex.
    let key_hex = hex::encode(signing_key.to_bytes());
    std::fs::write(output, &key_hex)?;

    println!("trader_id  : {}", hex::encode(pubkey.to_bytes()));
    println!("key file   : {}", output.display());
    println!();
    println!("Keep the key file secret — it is the only credential for this trader identity.");

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn cmd_place(
    client: &reqwest::Client,
    server: &str,
    market: String,
    side: String,
    price: u64,
    size: u64,
    key: PathBuf,
    client_id: Option<String>,
    nonce: Option<u64>,
) -> Result<(), Box<dyn std::error::Error>> {
    let signing_key = load_key(&key)?;
    let pubkey_bytes = signing_key.verifying_key().to_bytes();
    let trader_id = hex::encode(pubkey_bytes);

    let side = match side.to_lowercase().as_str() {
        "buy" | "b" => Side::Buy,
        "sell" | "s" => Side::Sell,
        other => return Err(format!("unknown side '{other}' — use 'buy' or 'sell'").into()),
    };

    let now_ms = unix_ms();
    let nonce = nonce.unwrap_or(now_ms);
    let client_order_id = client_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    let mut order = SignedOrder {
        schema_version: CURRENT_SCHEMA_VERSION,
        client_order_id,
        market_id: MarketId(market),
        trader_id,
        side,
        price_ticks: price,
        size_lots: size,
        time_in_force: TimeInForce::Gtc,
        nonce,
        expiry_ts_ms: 0,
        created_at_ms: now_ms,
        salt: rand::random(),
        trader_pubkey: pubkey_bytes,
        signature: [0u8; 64],
    };

    let hash = order.canonical_hash();
    order.signature = signing_key.sign(&hash.0).to_bytes();

    let resp = client
        .post(format!("{server}/v1/orders"))
        .json(&order)
        .send()
        .await?;

    let status = resp.status();
    let body: serde_json::Value = resp.json().await?;

    if status.is_success() {
        print_json(&body);
    } else {
        eprintln!("server error {status}:");
        print_json(&body);
        std::process::exit(1);
    }

    Ok(())
}

async fn cmd_admin(
    client: &reqwest::Client,
    server: &str,
    command: AdminCommand,
) -> Result<(), Box<dyn std::error::Error>> {
    match command {
        AdminCommand::Pause { market_id, admin_key, reason } => {
            let body = serde_json::json!({ "triggered_by": reason });
            let resp: serde_json::Value = client
                .post(format!("{server}/v1/admin/markets/{market_id}/pause"))
                .header("X-Admin-Key", &admin_key)
                .json(&body)
                .send()
                .await?
                .json()
                .await?;
            print_json(&resp);
        }

        AdminCommand::Resume { market_id, admin_key, reason } => {
            let body = serde_json::json!({ "triggered_by": reason });
            let resp: serde_json::Value = client
                .post(format!("{server}/v1/admin/markets/{market_id}/resume"))
                .header("X-Admin-Key", &admin_key)
                .json(&body)
                .send()
                .await?
                .json()
                .await?;
            print_json(&resp);
        }

        AdminCommand::CancelAll { market_id, admin_key } => {
            let resp: serde_json::Value = client
                .post(format!("{server}/v1/admin/markets/{market_id}/cancel-all"))
                .header("X-Admin-Key", &admin_key)
                .send()
                .await?
                .json()
                .await?;
            print_json(&resp);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Load an ed25519 signing key from a hex file produced by `clob keygen`.
fn load_key(path: &PathBuf) -> Result<SigningKey, Box<dyn std::error::Error>> {
    let hex = std::fs::read_to_string(path)
        .map_err(|e| format!("could not read key file '{}': {e}", path.display()))?;
    let bytes = hex::decode(hex.trim())
        .map_err(|e| format!("invalid hex in key file '{}': {e}", path.display()))?;
    let seed: [u8; 32] = bytes
        .try_into()
        .map_err(|_| format!("key file '{}' must contain exactly 32 bytes (64 hex chars)", path.display()))?;
    Ok(SigningKey::from_bytes(&seed))
}

/// Current Unix timestamp in milliseconds.
fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Pretty-print any JSON value to stdout.
fn print_json(v: &serde_json::Value) {
    println!("{}", serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string()));
}

/// Print the order book in a human-readable table.
fn print_book(v: &serde_json::Value) {
    let empty = vec![];
    let asks = v["asks"].as_array().unwrap_or(&empty);
    let bids = v["bids"].as_array().unwrap_or(&empty);

    // Print asks in reverse (lowest ask at the bottom, nearest to mid).
    println!("{:>14}  {:>14}", "price", "size");
    println!("{:-<14}  {:-<14}", "", "");
    for ask in asks.iter().rev() {
        println!("{:>14}  {:>14}  ask", ask["price_ticks"], ask["total_qty_lots"]);
    }
    println!("{:-<14}  {:-<14}", "", "");
    for bid in bids {
        println!("{:>14}  {:>14}  bid", bid["price_ticks"], bid["total_qty_lots"]);
    }
}
