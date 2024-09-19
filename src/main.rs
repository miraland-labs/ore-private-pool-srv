use {
    self::models::*,
    ::ore_utils::AccountDeserialize,
    axum::{
        debug_handler,
        extract::{
            ws::{Message, WebSocket},
            ConnectInfo, Query, State, WebSocketUpgrade,
        },
        http::{Method, Response, StatusCode},
        response::IntoResponse,
        routing::get,
        Extension, Router,
    },
    axum_extra::{headers::authorization::Basic, TypedHeader},
    base64::{prelude::BASE64_STANDARD, Engine},
    bitflags::bitflags,
    chrono::Local,
    clap::{
        builder::{
            styling::{AnsiColor, Effects},
            Styles,
        },
        command, Parser,
    },
    database::{Database, DatabaseError, PoweredByDbms, PoweredByParams},
    drillx::Solution,
    dynamic_fee as pfee,
    futures::{stream::SplitSink, SinkExt, StreamExt},
    notification::RewardsMessage,
    ore_api::{
        consts::{BUS_COUNT, EPOCH_DURATION},
        error::OreError,
        state::Proof,
    },
    rand::Rng,
    rusqlite::Connection,
    serde::Deserialize,
    solana_account_decoder::UiAccountEncoding,
    solana_client::{
        client_error::ClientErrorKind,
        nonblocking::{pubsub_client::PubsubClient, rpc_client::RpcClient},
        rpc_config::{RpcAccountInfoConfig, RpcSendTransactionConfig},
        send_and_confirm_transactions_in_parallel::SendAndConfirmConfig,
    },
    solana_sdk::{
        commitment_config::{CommitmentConfig, CommitmentLevel},
        compute_budget::ComputeBudgetInstruction,
        native_token::LAMPORTS_PER_SOL,
        pubkey::Pubkey,
        signature::{read_keypair_file, Keypair, Signature},
        signer::Signer,
        transaction::Transaction,
    },
    solana_transaction_status::UiTransactionEncoding,
    std::{
        collections::{HashMap, HashSet},
        // fs,
        net::SocketAddr,
        ops::{ControlFlow, Div, Range},
        path::Path,
        str::FromStr,
        sync::{
            atomic::{AtomicBool, Ordering::Relaxed},
            Arc, OnceLock,
        },
        time::{Duration, SystemTime, UNIX_EPOCH},
    },
    tokio::{
        sync::{
            mpsc::{UnboundedReceiver, UnboundedSender},
            Mutex, RwLock,
        },
        time::Instant,
    },
    tower_http::{
        cors::CorsLayer,
        trace::{DefaultMakeSpan, TraceLayer},
    },
    tracing::{debug, error, info, warn},
    utils::{
        get_auth_ix, get_clock, get_config_and_proof, get_cutoff, get_cutoff_with_risk,
        get_mine_ix, get_proof, get_register_ix, get_reset_ix, proof_pubkey, ORE_TOKEN_DECIMALS,
    },
};

mod database;
mod dynamic_fee;
mod models;
mod notification;
mod tpu;
mod utils;

// MI
// min hash power is matching with ore BASE_REWARD_RATE_MIN_THRESHOLD
// min difficulty, matching with MIN_HASHPOWER.
const MIN_HASHPOWER: u64 = 5;
const MIN_DIFF: u32 = 5;

// MI: if 0, rpc node will retry the tx until it is finalized or until the blockhash expires
const RPC_RETRIES: usize = 3; // 5

const SUBMIT_LIMIT: u32 = 5;
const CHECK_LIMIT: usize = 30; // 30
const NO_BEST_SOLUTION_INTERVAL: usize = 5;

static POWERED_BY_DBMS: OnceLock<PoweredByDbms> = OnceLock::new();

#[derive(Clone)]
struct ClientConnection {
    pubkey: Pubkey,
    miner_id: i64,
    socket: Arc<Mutex<SplitSink<WebSocket, Message>>>,
}

// #[derive(Clone)]
struct WalletExtension {
    miner_wallet: Arc<Keypair>,
    #[allow(dead_code)]
    fee_wallet: Arc<Keypair>,
}

struct AppState {
    sockets: HashMap<SocketAddr, ClientConnection>,
}

pub struct MessageInternalAllClients {
    text: String,
}

#[derive(Debug, Clone, Copy)]
pub struct InternalMessageContribution {
    miner_id: i64,
    supplied_diff: u32,
    supplied_nonce: u64,
    hashpower: u64,
}

pub struct MessageInternalMineSuccess {
    difficulty: u32,
    total_balance: f64,
    rewards: i64,
    challenge_id: i64,
    challenge: [u8; 32],
    best_nonce: u64,
    total_hashpower: u64,
    ore_config: Option<ore_api::state::Config>,
    multiplier: f64,
    contributions: HashMap<Pubkey, InternalMessageContribution>,
}

pub struct LastPong {
    pongs: HashMap<SocketAddr, Instant>,
}

#[derive(Debug)]
pub enum ClientMessage {
    Ready(SocketAddr),
    Mining(SocketAddr),
    Pong(SocketAddr),
    BestSolution(SocketAddr, Solution, Pubkey),
}

pub struct EpochHashes {
    challenge: [u8; 32],
    best_hash: BestHash,
    contributions: HashMap<Pubkey, InternalMessageContribution>,
}

pub struct BestHash {
    solution: Option<Solution>,
    difficulty: u32,
}

pub struct MineConfig {
    // mining pool db table rowid/identity if powered by dbms
    pool_id: i32,
}

bitflags! {
    #[repr(transparent)]
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct MessagingFlags: u8 {
        const SLACK   = 1 << 0;
        const DISCORD = 1 << 1;
        // const EMAIL   = 1 << 2;
    }
}

pub struct DifficultyPayload {
    pub solution_difficulty: u32,
    pub expected_min_difficulty: u32,
    pub extra_fee_difficulty: u32,
    pub extra_fee_percent: u64,
}

#[derive(Parser, Debug)]
#[command(version, author, about, long_about = None, styles = styles())]
struct Args {
    #[arg(
        long,
        short,
        value_name = "BUFFER_SECONDS",
        help = "The number seconds before the deadline to stop mining and start submitting.",
        default_value = "5"
    )]
    pub buffer_time: u64,

    #[arg(
        long,
        short,
        value_name = "RISK_SECONDS",
        help = "Set extra hash time in seconds for miners to stop mining and start submitting, risking a penalty.",
        default_value = "0"
    )]
    pub risk_time: u64,

    #[arg(
        long,
        value_name = "FEE_MICROLAMPORTS",
        help = "Price to pay for compute units when dynamic fee flag is off, or dynamic fee is unavailable.",
        default_value = "100",
        global = true
    )]
    priority_fee: Option<u64>,

    #[arg(
        long,
        value_name = "FEE_CAP_MICROLAMPORTS",
        help = "Max price to pay for compute units when dynamic fees are enabled.",
        default_value = "100000",
        global = true
    )]
    priority_fee_cap: Option<u64>,

    #[arg(long, help = "Enable dynamic priority fees", global = true)]
    dynamic_fee: bool,

    #[arg(
        long,
        value_name = "DYNAMIC_FEE_URL",
        help = "RPC URL to use for dynamic fee estimation.",
        global = true
    )]
    dynamic_fee_url: Option<String>,

    #[arg(
        long,
        short,
        value_name = "EXPECTED_MIN_DIFFICULTY",
        help = "The expected min difficulty to submit from pool client. Reserved for potential qualification process unimplemented yet.",
        default_value = "8"
    )]
    pub expected_min_difficulty: u32,

    #[arg(
        long,
        short,
        value_name = "EXTRA_FEE_DIFFICULTY",
        help = "The min difficulty that the pool server miner thinks deserves to pay more priority fee to land tx quickly.",
        default_value = "29"
    )]
    pub extra_fee_difficulty: u32,

    #[arg(
        long,
        short,
        value_name = "EXTRA_FEE_PERCENT",
        help = "The extra percentage that the pool server miner feels deserves to pay more of the priority fee. As a percentage, a multiple of 50 is recommended(example: 50, means pay extra 50% of the specified priority fee), and the final priority fee cannot exceed the priority fee cap.",
        default_value = "0"
    )]
    pub extra_fee_percent: u64,

    #[arg(
        long,
        short,
        value_name = "SLACK_DIFFICULTY",
        help = "The min difficulty that will notify slack channel(if configured) upon transaction success. It's deprecated in favor of messaging_diff",
        default_value = "25"
    )]
    pub slack_difficulty: u32,

    #[arg(
        long,
        short,
        value_name = "MESSAGING_DIFF",
        help = "The min difficulty that will notify messaging channels(if configured) upon transaction success.",
        default_value = "25"
    )]
    pub messaging_diff: u32,

    #[arg(long, help = "Send and confirm transactions using tpu client.", global = true)]
    send_tpu_mine_tx: bool,

    /// Mine with sound notification on/off
    #[arg(
        long,
        value_name = "NO_SOUND_NOTIFICATION",
        help = "Sound notification on by default",
        default_value = "false",
        global = true
    )]
    pub no_sound_notification: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    static PAUSED: AtomicBool = AtomicBool::new(false);
    color_eyre::install().unwrap();
    dotenv::dotenv().ok();
    let args = Args::parse();

    let filter_layer = tracing_subscriber::EnvFilter::try_from_default_env()
        .or_else(|_| tracing_subscriber::EnvFilter::try_new("info"))
        .unwrap();

    let file_appender = tracing_appender::rolling::daily("./logs", "ore-ppl-srv.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);
    tracing_subscriber::fmt().with_env_filter(filter_layer).with_writer(non_blocking).init();

    // load envs
    let wallet_path_str = std::env::var("WALLET_PATH").expect("WALLET_PATH must be set.");
    let key = "FEE_WALLET_PATH";
    let fee_wallet_path_str = match std::env::var(key) {
        Ok(val) => val,
        Err(_) => {
            info!("FEE_WALLET_PATH not set, using WALLET_PATH instead.");
            wallet_path_str.clone()
        },
    };
    let rpc_url = std::env::var("RPC_URL").expect("RPC_URL must be set.");
    let rpc_ws_url = std::env::var("RPC_WS_URL").expect("RPC_WS_URL must be set.");

    // let mut powered_by_dbms = PoweredByDbms::Unavailable;
    // let key = "POWERED_BY_DBMS";
    // match std::env::var(key) {
    //     Ok(val) => {
    //         powered_by_dbms =
    //             PoweredByDbms::from_str(&val).expect("POWERED_BY_DBMS must be set correctly.");
    //     },
    //     Err(e) => {},
    // }

    let powered_by_dbms = POWERED_BY_DBMS.get_or_init(|| {
        let key = "POWERED_BY_DBMS";
        match std::env::var(key) {
            Ok(val) =>
                PoweredByDbms::from_str(&val).expect("POWERED_BY_DBMS must be set correctly."),
            Err(_) => PoweredByDbms::Unavailable,
        }
    });

    let database_uri: String;
    let key = "DATABASE_URL";
    match std::env::var(key) {
        Ok(val) => {
            database_uri = val;
        },
        Err(_) => database_uri = String::from("ore_priv_pool.db.sqlite3"),
    }

    let reports_interval_in_hrs: u64 = match std::env::var("REPORTS_INTERVAL_IN_HOURS") {
        Ok(val) => val.parse().expect("REPORTS_INTERVAL_IN_HOURS must be a positive number"),
        Err(_) => 6,
    };

    let mut dbms_settings = PoweredByParams {
        // default to "./ore_priv_pool.db.sqlite3"
        database_uri: &database_uri,
        initialized: false,
        corrupted: false,
        // connection: None,
    };

    if powered_by_dbms == &PoweredByDbms::Sqlite {
        info!("Powered by {} detected.", powered_by_dbms);
        // First, let's check if db file exist or not
        if !utils::exists_file(&dbms_settings.database_uri) {
            warn!(
                "No existing database! New database will be created in the path: {}",
                dbms_settings.database_uri
            );
        } else {
            info!(
                "The ore private pool db is already in place: {}. Opening...",
                dbms_settings.database_uri
            );
            dbms_settings.initialized = true;
        }
        // Second, we try to open a database connection.
        let conn = match Connection::open(dbms_settings.database_uri) {
            Ok(conn) => conn,
            Err(e) => {
                error!("Error connecting to database: {}.", e);
                return Err("Failed to connect to database.".into());
            },
        };

        // initialization check
        if !dbms_settings.initialized {
            info!("Initializing database...");
            // execute db init sql scripts
            // let command = fs::read_to_string("migrations/sqlite/init.sql").unwrap();
            let command = include_str!("../migrations/sqlite/init.sql");
            // conn.execute_batch(&command).unwrap();
            if let Err(e) = conn.execute_batch(&command) {
                error!("Error occurred during db initialization: {}", e);
                return Err("Failed during db initialization.".into());
            }
            dbms_settings.initialized = true;
            info!("Initialization completed.");
        }

        // retrive initialization completed flag
        let mut stmt =
            conn.prepare("SELECT id FROM init_completion WHERE init_completed = true")?;
        dbms_settings.corrupted = !stmt.exists([]).unwrap();

        // db file corruption check
        if dbms_settings.corrupted {
            error!("ore private pool db file corrupted.");
            return Err("ore private pool db file corrupted.".into());
        }
    }

    let database = Arc::new(Database::new(dbms_settings.database_uri.to_string()));

    //TODO: further verify those webhook existences are valid
    let mut exists_slack_webhook = false;
    let mut slack_webhook = String::new();

    let key = "SLACK_WEBHOOK";
    match std::env::var(key) {
        Ok(val) => {
            exists_slack_webhook = true;
            slack_webhook = val;
        },
        Err(e) => warn!("couldn't interpret {key}: {e}. slack messaging service unvailable."),
    }

    let mut exists_discord_webhook = false;
    let mut discord_webhook = String::new();
    let key = "DISCORD_WEBHOOK";
    match std::env::var(key) {
        Ok(val) => {
            exists_discord_webhook = true;
            discord_webhook = val;
        },
        Err(e) => warn!("couldn't interpret {key}: {e}. discord messaging service unvailable."),
    }

    let mut messaging_flags = MessagingFlags::empty();
    if exists_slack_webhook {
        messaging_flags |= MessagingFlags::SLACK;
    }
    if exists_discord_webhook {
        messaging_flags |= MessagingFlags::DISCORD;
    }

    let priority_fee = Arc::new(args.priority_fee);
    let priority_fee_cap = Arc::new(args.priority_fee_cap);

    let buffer_time = Arc::new(args.buffer_time);
    let risk_time = Arc::new(args.risk_time);

    let min_difficulty = Arc::new(args.expected_min_difficulty);
    let extra_fee_difficulty = Arc::new(args.extra_fee_difficulty);
    let extra_fee_percent = Arc::new(args.extra_fee_percent);

    let dynamic_fee = Arc::new(args.dynamic_fee);
    let dynamic_fee_url = Arc::new(args.dynamic_fee_url);

    let slack_difficulty = Arc::new(args.slack_difficulty);
    let messaging_diff = Arc::new(args.messaging_diff);

    let send_tpu_mine_tx = Arc::new(args.send_tpu_mine_tx);

    let no_sound_notification = Arc::new(args.no_sound_notification);

    // load wallet
    let wallet_path = Path::new(&wallet_path_str);

    if !wallet_path.exists() {
        tracing::error!("❌ Failed to load wallet at: {}", wallet_path_str);
        return Err("Failed to find wallet path.".into());
    }

    let wallet = read_keypair_file(wallet_path)
        .expect("Failed to load keypair from file: {wallet_path_str}");
    let wallet_pubkey = wallet.pubkey();
    info!("loaded wallet {}", wallet_pubkey.to_string());

    // load fee wallet
    let wallet_path = Path::new(&fee_wallet_path_str);

    if !wallet_path.exists() {
        tracing::error!("❌ Failed to load fee wallet at: {}", fee_wallet_path_str);
        return Err("Failed to find fee wallet path.".into());
    }

    let fee_wallet = read_keypair_file(wallet_path)
        .expect("Failed to load keypair from file: {wallet_path_str}");
    info!("loaded fee wallet {}", wallet.pubkey().to_string());

    info!("establishing rpc connection...");
    let rpc_client = RpcClient::new_with_commitment(rpc_url, CommitmentConfig::confirmed());

    info!("loading sol balance...");
    let balance = if let Ok(balance) = rpc_client.get_balance(&wallet_pubkey).await {
        balance
    } else {
        return Err("Failed to load balance".into());
    };

    info!("Balance: {:.9}", balance as f64 / LAMPORTS_PER_SOL as f64);

    if balance < 1_000_000 {
        return Err("Sol balance is too low!".into());
    }

    // MI
    let proof_pubkey = proof_pubkey(wallet_pubkey);
    debug!("PROOF ADDRESS: {:?}", proof_pubkey);
    let proof = if let Ok(loaded_proof) = get_proof(&rpc_client, wallet_pubkey).await {
        debug!("LOADED PROOF: \n{:?}", loaded_proof);
        loaded_proof
    } else {
        error!("Failed to load proof.");
        info!("Creating proof account...");

        let ix = get_register_ix(wallet_pubkey);

        if let Ok((hash, _slot)) =
            rpc_client.get_latest_blockhash_with_commitment(rpc_client.commitment()).await
        {
            let mut tx = Transaction::new_with_payer(&[ix], Some(&wallet_pubkey));

            tx.sign(&[&wallet], hash);

            let result = rpc_client
                .send_and_confirm_transaction_with_spinner_and_commitment(
                    &tx,
                    rpc_client.commitment(),
                )
                .await;

            if let Ok(sig) = result {
                info!("Sig: {}", sig.to_string());
            } else {
                return Err("Failed to create proof account".into());
            }
        }
        let proof = if let Ok(loaded_proof) = get_proof(&rpc_client, wallet_pubkey).await {
            loaded_proof
        } else {
            return Err("Failed to get newly created proof".into());
        };
        proof
    };

    let mine_config: Arc<MineConfig>;
    if powered_by_dbms == &PoweredByDbms::Sqlite {
        info!("Check if the mining pool record exists in the database");
        let mining_pool = database.get_pool_by_authority_pubkey(wallet_pubkey.to_string()).await;

        match mining_pool {
            Ok(_) => {},
            Err(DatabaseError::FailedToGetConnectionFromPool) => {
                panic!("Failed to get a connection from database pool");
            },
            Err(_) => {
                info!("Mining pool record missing from database. Inserting...");
                let proof_pubkey = utils::proof_pubkey(wallet_pubkey);
                let result = database
                    .add_new_pool(wallet_pubkey.to_string(), proof_pubkey.to_string())
                    .await;

                if result.is_err() {
                    panic!("Failed to add mining pool record in database");
                } else {
                    info!("Mining pool record added to database");
                }
            },
        }
        // info!("Mining pool record added to database");
        let mining_pool =
            database.get_pool_by_authority_pubkey(wallet_pubkey.to_string()).await.unwrap();

        mine_config = Arc::new(MineConfig { pool_id: mining_pool.id });

        info!("Check if current challenge for the pool exists in the database");
        let challenge = database.get_challenge_by_challenge(proof.challenge.to_vec()).await;

        match challenge {
            Ok(_) => {},
            Err(DatabaseError::FailedToGetConnectionFromPool) => {
                panic!("Failed to get a connection from database pool");
            },
            Err(_) => {
                info!("Challenge record missing from database. Inserting...");
                let new_challenge = models::InsertChallenge {
                    pool_id: mining_pool.id,
                    challenge: proof.challenge.to_vec(),
                    rewards_earned: None,
                };
                let result = database.add_new_challenge(new_challenge).await;

                if result.is_err() {
                    panic!("Failed to add challenge record in database");
                } else {
                    info!("Challenge record added to database");
                }
            },
        }
    } else {
        mine_config = Arc::new(MineConfig { pool_id: i32::MAX });
    }

    let epoch_hashes = Arc::new(RwLock::new(EpochHashes {
        challenge: proof.challenge,
        best_hash: BestHash { solution: None, difficulty: 0 },
        contributions: HashMap::new(),
    }));

    // let wallet_extension = Arc::new(wallet);
    let wallet_extension = Arc::new(WalletExtension {
        miner_wallet: Arc::new(wallet),
        fee_wallet: Arc::new(fee_wallet),
    });
    let proof_ext = Arc::new(Mutex::new(proof));
    let nonce_ext = Arc::new(Mutex::new(0u64));

    let client_nonce_ranges = Arc::new(RwLock::new(HashMap::new()));

    let shared_state = Arc::new(RwLock::new(AppState { sockets: HashMap::new() }));
    let ready_clients = Arc::new(Mutex::new(HashSet::new()));

    let pongs = Arc::new(RwLock::new(LastPong { pongs: HashMap::new() }));

    // Track client pong timings
    let app_pongs = pongs.clone();
    let app_state = shared_state.clone();
    tokio::spawn(async move {
        pong_tracking_system(app_pongs, app_state).await;
    });

    let app_wallet = wallet_extension.clone();
    let app_proof = proof_ext.clone();
    // Establish webocket connection for tracking pool proof changes.
    tokio::spawn(async move {
        proof_tracking_system(rpc_ws_url, app_wallet.miner_wallet.clone(), app_proof).await;
    });

    let (client_message_sender, client_message_receiver) =
        tokio::sync::mpsc::unbounded_channel::<ClientMessage>();

    // Handle client messages
    let app_ready_clients = ready_clients.clone();
    let app_proof = proof_ext.clone();
    let app_epoch_hashes = epoch_hashes.clone();
    let app_client_nonce_ranges = client_nonce_ranges.clone();
    let app_min_difficulty = min_difficulty.clone();
    let app_state = shared_state.clone();
    let app_pongs = pongs.clone();
    tokio::spawn(async move {
        client_message_handler_system(
            app_state,
            client_message_receiver,
            app_epoch_hashes,
            app_ready_clients,
            app_proof,
            app_client_nonce_ranges,
            app_pongs,
            *app_min_difficulty,
        )
        .await;
    });

    let (slack_message_sender, slack_message_receiver) =
        tokio::sync::mpsc::unbounded_channel::<RewardsMessage>();

    // if exists slack webhook, handle slack messages to send
    if messaging_flags.contains(MessagingFlags::SLACK) {
        tokio::spawn(async move {
            notification::slack_messaging_system(slack_webhook, slack_message_receiver).await;
        });
    }

    let (discord_message_sender, discord_message_receiver) =
        tokio::sync::mpsc::unbounded_channel::<RewardsMessage>();

    // if exists discord webhook, handle discord messages to send
    // if exists_discord_webhook {
    if messaging_flags.contains(MessagingFlags::DISCORD) {
        tokio::spawn(async move {
            notification::discord_messaging_system(discord_webhook, discord_message_receiver).await;
        });
    }

    // Start report routine
    let app_mine_config = mine_config.clone();
    let app_database = database.clone();
    tokio::spawn(async move {
        reporting_system(reports_interval_in_hrs, app_mine_config, app_database).await;
    });

    // Handle ready clients
    let rpc_client = Arc::new(rpc_client);
    let app_rpc_client = rpc_client.clone();
    let app_ready_clients = ready_clients.clone();
    let app_shared_state = shared_state.clone();
    let app_proof = proof_ext.clone();
    let app_epoch_hashes = epoch_hashes.clone();
    let app_buffer_time = buffer_time.clone();
    let app_risk_time = risk_time.clone();
    let app_nonce = nonce_ext.clone();
    let app_client_nonce_ranges = client_nonce_ranges.clone();
    tokio::spawn(async move {
        let rpc_client = app_rpc_client;
        let ready_clients = app_ready_clients.clone();
        loop {
            let mut clients = Vec::new();
            {
                let ready_clients_lock = ready_clients.lock().await;
                for ready_client in ready_clients_lock.iter() {
                    clients.push(ready_client.clone());
                }
                drop(ready_clients_lock);
            };

            if !PAUSED.load(Relaxed) && clients.len() > 0 {
                let lock = app_proof.lock().await;
                let proof = lock.clone();
                drop(lock);

                let cutoff = if (*app_risk_time).gt(&0) {
                    get_cutoff_with_risk(&rpc_client, proof, *app_buffer_time, *app_risk_time).await
                } else {
                    get_cutoff(&rpc_client, proof, *app_buffer_time).await
                };
                let mut should_mine = true;
                let cutoff = if cutoff <= 0 {
                    let solution = app_epoch_hashes.read().await.best_hash.solution;
                    if solution.is_some() {
                        should_mine = false;
                    }
                    0
                } else {
                    cutoff
                };

                if should_mine {
                    let lock = app_proof.lock().await;
                    let proof = lock.clone();
                    drop(lock);
                    let challenge = proof.challenge;
                    info!(
                        "Mission to clients with challenge: {}",
                        BASE64_STANDARD.encode(challenge)
                    );
                    info!("and cutoff in: {}s", cutoff);
                    for client in clients {
                        let nonce_range = {
                            let mut nonce = app_nonce.lock().await;
                            let start = *nonce;
                            // max hashes possible in 60s for a single client
                            *nonce += 4_000_000;
                            drop(nonce);
                            // max hashes possible in 60s for a single client
                            //
                            let nonce_end = start + 3_999_999;
                            let end = nonce_end;
                            start..end
                        };
                        // message type is 8 bits = 1 u8
                        // challenge is 256 bits = 32 u8
                        // cutoff is 64 bits = 8 u8
                        // nonce_range is 128 bits, start is 64 bits, end is 64 bits = 16 u8
                        let mut bin_data = [0; 57];
                        bin_data[00..1].copy_from_slice(&0u8.to_le_bytes());
                        bin_data[01..33].copy_from_slice(&challenge);
                        bin_data[33..41].copy_from_slice(&cutoff.to_le_bytes());
                        bin_data[41..49].copy_from_slice(&nonce_range.start.to_le_bytes());
                        bin_data[49..57].copy_from_slice(&nonce_range.end.to_le_bytes());

                        let app_client_nonce_ranges = app_client_nonce_ranges.clone();
                        let shared_state = app_shared_state.read().await;
                        let sockets = shared_state.sockets.clone();
                        drop(shared_state);
                        if let Some(sender) = sockets.get(&client) {
                            let sender = sender.clone();
                            let ready_clients = ready_clients.clone();
                            tokio::spawn(async move {
                                let _ = sender
                                    .socket
                                    .lock()
                                    .await
                                    .send(Message::Binary(bin_data.to_vec()))
                                    .await;
                                let _ = ready_clients.lock().await.remove(&client);
                                let _ = app_client_nonce_ranges
                                    .write()
                                    .await
                                    .insert(sender.pubkey, nonce_range);
                            });
                        }
                    }
                }
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    });

    let (mine_success_sender, mut mine_success_receiver) =
        tokio::sync::mpsc::unbounded_channel::<MessageInternalMineSuccess>();

    let (all_clients_sender, mut all_clients_receiver) =
        tokio::sync::mpsc::unbounded_channel::<MessageInternalAllClients>();

    // let rpc_client = Arc::new(rpc_client); // delcared in previous
    let app_rpc_client = rpc_client.clone();
    let app_mine_config = mine_config.clone();
    let app_shared_state = shared_state.clone();
    let app_proof = proof_ext.clone();
    let app_epoch_hashes = epoch_hashes.clone();
    let app_wallet = wallet_extension.clone();
    let app_nonce = nonce_ext.clone();
    let app_dynamic_fee = dynamic_fee.clone();
    let app_dynamic_fee_url = dynamic_fee_url.clone();
    let app_priority_fee = priority_fee.clone();
    let app_priority_fee_cap = priority_fee_cap.clone();
    let app_extra_fee_difficulty = extra_fee_difficulty.clone();
    let app_extra_fee_percent = extra_fee_percent.clone();
    let app_send_tpu_mine_tx = send_tpu_mine_tx.clone();
    let app_no_sound_notification = no_sound_notification.clone();
    let app_database = database.clone();
    let app_all_clients_sender = all_clients_sender.clone();
    let app_slack_message_sender = slack_message_sender.clone();
    let app_discord_message_sender = discord_message_sender.clone();
    let app_slack_difficulty = slack_difficulty.clone();
    let app_messaging_diff = messaging_diff.clone();
    let app_buffer_time = buffer_time.clone();
    let app_risk_time = risk_time.clone();
    tokio::spawn(async move {
        let rpc_client = app_rpc_client;
        let mine_config = app_mine_config;
        let database = app_database;
        let send_tpu_mine_tx = app_send_tpu_mine_tx;
        let slack_message_sender = app_slack_message_sender;
        let discord_message_sender = app_discord_message_sender;
        let slack_difficulty = app_slack_difficulty;
        let messaging_diff = app_messaging_diff;
        // MI
        let mut solution_is_none_counter = 0;
        let mut num_waiting = 0;
        loop {
            let lock = app_proof.lock().await;
            let old_proof = lock.clone();
            drop(lock);

            debug!("We are lucky! No deadlock of app_proof.");

            let cutoff = if (*app_risk_time).gt(&0) {
                get_cutoff_with_risk(&rpc_client, old_proof, *app_buffer_time, *app_risk_time).await
            } else {
                get_cutoff(&rpc_client, old_proof, *app_buffer_time).await
            };
            debug!("Start new loop. Let's check current cutoff value: {cutoff}");
            if cutoff <= 0_i64 {
                if cutoff <= -(*app_buffer_time as i64) {
                    // prepare to process solution
                    let reader = app_epoch_hashes.read().await;
                    let solution = reader.best_hash.solution.clone();
                    drop(reader);

                    let shared_state_lock = app_shared_state.read().await;
                    let num_active_miners = shared_state_lock.sockets.len();
                    drop(shared_state_lock);

                    // start to process solution
                    if solution.is_some() {
                        let signer = app_wallet.clone().miner_wallet.clone();

                        let bus = rand::thread_rng().gen_range(0..BUS_COUNT);

                        let mut success = false;
                        let reader = app_epoch_hashes.read().await;
                        let best_solution = reader.best_hash.solution.clone();
                        let contributions = reader.contributions.clone();
                        let num_contributions = contributions.len();
                        // MI, wait until all active miners' submissions received, and waiting times
                        // < 6. if min_difficulty is relative high, may
                        // always false num_contributions depends on min diff
                        if num_contributions != num_active_miners && num_waiting < 6 {
                            tokio::time::sleep(Duration::from_millis(500)).await;
                            num_waiting += 1;
                            continue;
                        } else {
                            // reset waiting times
                            num_waiting = 0;
                        }
                        drop(reader);

                        // set mining pause flag before submitting best solution
                        info!("pause new mining mission");
                        PAUSED.store(true, Relaxed);

                        for i in 0..SUBMIT_LIMIT {
                            if let Some(best_solution) = best_solution {
                                let difficulty = best_solution.to_hash().difficulty();

                                info!(
                                    "Submitting attempt {} with ✨ diff {} ✨ of {} qualified contributions at {}.",
                                    i + 1,
                                    difficulty,
                                    num_contributions,
                                    Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
                                );
                                info!(
                                    "Submission Challenge: {}",
                                    BASE64_STANDARD.encode(old_proof.challenge)
                                );

                                info!("Getting current/latest config and proof.");
                                let ore_config = if let Ok((loaded_config, loaded_proof)) =
                                    get_config_and_proof(&rpc_client, signer.pubkey()).await
                                {
                                    info!(
                                        "Current/latest pool Challenge: {}",
                                        BASE64_STANDARD.encode(loaded_proof.challenge)
                                    );

                                    if !best_solution.is_valid(&loaded_proof.challenge) {
                                        error!("❌ SOLUTION IS NOT VALID ANYMORE!");
                                        let mut lock = app_proof.lock().await;
                                        *lock = loaded_proof;
                                        drop(lock);
                                        break;
                                    }

                                    Some(loaded_config)
                                } else {
                                    None
                                };

                                let _now = SystemTime::now()
                                    .duration_since(UNIX_EPOCH)
                                    .expect("Time went backwards")
                                    .as_secs();

                                let clock = get_clock(&rpc_client).await;
                                let current_timestamp = clock.unix_timestamp;
                                let mut ixs = vec![];
                                let _ = app_all_clients_sender.send(MessageInternalAllClients {
                                    text: String::from("Server is sending mining transaction..."),
                                });

                                let mut cu_limit = 480_000;
                                let should_add_reset_ix = if let Some(config) = ore_config {
                                    let time_to_reset = (config.last_reset_at + EPOCH_DURATION)
                                        - current_timestamp as i64;
                                    if time_to_reset <= 5 {
                                        cu_limit = 500_000;
                                        info!("Including reset tx.");
                                        true
                                    } else {
                                        false
                                    }
                                } else {
                                    false
                                };

                                let cu_limit_ix =
                                    ComputeBudgetInstruction::set_compute_unit_limit(cu_limit);
                                ixs.push(cu_limit_ix);

                                let mut fee_type: &str = "static";
                                let fee: u64 = if *app_dynamic_fee {
                                    fee_type = "estimate";
                                    match pfee::dynamic_fee(
                                        &rpc_client,
                                        (*app_dynamic_fee_url).clone(),
                                        *app_priority_fee_cap,
                                    )
                                    .await
                                    {
                                        Ok(fee) => {
                                            let mut prio_fee = fee;
                                            // MI: calc uplimit of priority fee for precious diff
                                            {
                                                let best_solution_difficulty =
                                                    best_solution.to_hash().difficulty();
                                                if best_solution_difficulty
                                                    >= *app_extra_fee_difficulty
                                                {
                                                    prio_fee =
                                                        if let Some(ref app_priority_fee_cap) =
                                                            *app_priority_fee_cap
                                                        {
                                                            (*app_priority_fee_cap).min(
                                                                prio_fee
                                                                    .saturating_mul(
                                                                        100u64.saturating_add(
                                                                            *app_extra_fee_percent,
                                                                        ),
                                                                    )
                                                                    .saturating_div(100),
                                                            )
                                                        } else {
                                                            // No priority_fee_cap was set
                                                            // not exceed 300K
                                                            300_000.min(
                                                                prio_fee
                                                                    .saturating_mul(
                                                                        100u64.saturating_add(
                                                                            *app_extra_fee_percent,
                                                                        ),
                                                                    )
                                                                    .saturating_div(100),
                                                            )
                                                        }
                                                }
                                            }
                                            prio_fee
                                        },
                                        Err(err) => {
                                            let fee = app_priority_fee.unwrap_or(0);
                                            info!(
                                            "Error: {} Falling back to static value: {} microlamports",
                                            err, fee
                                        );
                                            fee
                                        },
                                    }
                                } else {
                                    // static
                                    // MI, vanilla: if stick to static/fix price no matter diff, use
                                    // this stmt.
                                    // app_priority_fee.unwrap_or(0)

                                    // MI: consider to pay more fee for precious diff even with
                                    // static fee mode
                                    let mut prio_fee = app_priority_fee.unwrap_or(0);
                                    // MI: calc uplimit of priority fee for precious diff
                                    {
                                        let best_solution_difficulty =
                                            best_solution.to_hash().difficulty();
                                        if best_solution_difficulty >= *app_extra_fee_difficulty {
                                            prio_fee =
                                                if let Some(ref app_priority_fee_cap) =
                                                    *app_priority_fee_cap
                                                {
                                                    (*app_priority_fee_cap).min(
                                                        prio_fee
                                                            .saturating_mul(100u64.saturating_add(
                                                                *app_extra_fee_percent,
                                                            ))
                                                            .saturating_div(100),
                                                    )
                                                } else {
                                                    // No priority_fee_cap was set
                                                    // not exceed 300K
                                                    300_000.min(
                                                        prio_fee
                                                            .saturating_mul(100u64.saturating_add(
                                                                *app_extra_fee_percent,
                                                            ))
                                                            .saturating_div(100),
                                                    )
                                                }
                                        }
                                    }
                                    prio_fee
                                };

                                let prio_fee_ix =
                                    ComputeBudgetInstruction::set_compute_unit_price(fee);
                                ixs.push(prio_fee_ix);

                                let noop_ix = get_auth_ix(signer.pubkey());
                                ixs.push(noop_ix);

                                if should_add_reset_ix {
                                    let reset_ix = get_reset_ix(signer.pubkey());
                                    ixs.push(reset_ix);
                                }

                                let ix_mine = get_mine_ix(signer.pubkey(), best_solution, bus);
                                ixs.push(ix_mine);

                                // so far all ixs are constructed, next submit-and-confirm
                                if *send_tpu_mine_tx {
                                    info!("Send tpu mine tx flag is on.");
                                    let config = SendAndConfirmConfig {
                                        resign_txs_count: Some(5),
                                        with_spinner: true,
                                    };
                                    match tpu::send_and_confirm(&rpc_client, &ixs, &*signer, config)
                                        .await
                                    {
                                        Ok(_) => {
                                            success = true;
                                            info!("✅ Success!!");
                                        },
                                        Err(e) => {
                                            error!(
                                                "Error occurred within tpu::send_and_confirm: {}",
                                                e
                                            )
                                        },
                                    }
                                } else {
                                    info!("Send tpu mine tx flag is off. Use RPC call instead.");
                                    // vanilla rpc approach
                                    let send_cfg = RpcSendTransactionConfig {
                                        skip_preflight: true,
                                        preflight_commitment: Some(CommitmentLevel::Confirmed),
                                        encoding: Some(UiTransactionEncoding::Base64),
                                        max_retries: Some(RPC_RETRIES),
                                        min_context_slot: None,
                                    };

                                    if let Ok((hash, _slot)) = rpc_client
                                        .get_latest_blockhash_with_commitment(
                                            rpc_client.commitment(),
                                        )
                                        .await
                                    {
                                        let mut tx = Transaction::new_with_payer(
                                            &ixs,
                                            Some(&signer.pubkey()),
                                        );

                                        tx.sign(&[&signer], hash);
                                        info!(
                                            "Sending signed tx... with {} priority fee {}",
                                            fee_type, fee
                                        );
                                        info!("attempt: {}", i + 1);
                                        match rpc_client
                                            .send_and_confirm_transaction_with_spinner_and_config(
                                                &tx,
                                                rpc_client.commitment(),
                                                send_cfg,
                                            )
                                            .await
                                        {
                                            Ok(sig) => {
                                                // success
                                                success = true;
                                                info!("✅ Success!!");
                                                info!("Sig: {}", sig);

                                                if powered_by_dbms == &PoweredByDbms::Sqlite {
                                                    // spawn task #1: Txn writer
                                                    let itxn = InsertTransaction {
                                                        transaction_type: "mine".to_string(),
                                                        signature: sig.to_string(),
                                                        priority_fee: fee as i32,
                                                        pool_id: mine_config.pool_id,
                                                    };
                                                    let app_db = database.clone();
                                                    tokio::spawn(async move {
                                                        while let Err(_) = app_db
                                                            .add_new_transaction(itxn.clone())
                                                            .await
                                                        {
                                                            error!("Failed to add tx record to db! Retrying...");
                                                            tokio::time::sleep(
                                                                Duration::from_millis(1000),
                                                            )
                                                            .await;
                                                        }
                                                    });
                                                }
                                            },

                                            Err(err) => {
                                                match err.kind {
                                                    ClientErrorKind::TransactionError(solana_sdk::transaction::TransactionError::InstructionError(_, err)) => {
                                                        match err {
                                                            // Custom instruction error, parse into OreError
                                                            solana_program::instruction::InstructionError::Custom(err_code) => {
                                                                match err_code {
                                                                    e if e == OreError::NeedsReset as u32 => {
                                                                        error!("Ore: The epoch has ended and needs reset. Retrying...");
                                                                        continue;
                                                                    }
                                                                    e if e == OreError::HashInvalid as u32 => {
                                                                        error!("❌ Ore: The provided hash is invalid. See you next solution.");

                                                                        // break for (0..SUBMIT_LIMIT), re-enter outer loop to restart
                                                                        break;
                                                                    }
                                                                    _ => {
                                                                        error!("{}", &err.to_string());
                                                                        continue;
                                                                    }
                                                                }
                                                            },

                                                            // Non custom instruction error, return
                                                            _ => {
                                                                error!("{}", &err.to_string());
                                                            }
                                                        }
                                                    }

                                                    // MI: other error like what?
                                                    _ => {
                                                        error!("{}", &err.to_string());
                                                    }
                                                }
                                                // TODO: is sleep here necessary?, MI
                                                tokio::time::sleep(Duration::from_millis(100)).await
                                            },
                                        }
                                    } else {
                                        error!("Failed to get latest blockhash. retrying...");
                                        tokio::time::sleep(Duration::from_millis(1_000)).await;
                                    }
                                } // end mine tx route

                                if success {
                                    // spawn task #2: landed tx attender
                                    let (mission_completed_sender, mission_completed_receiver) =
                                        tokio::sync::oneshot::channel::<u8>();
                                    let app_app_mine_success_sender = mine_success_sender.clone();
                                    let app_app_nonce = app_nonce.clone();
                                    let app_app_database = database.clone();
                                    let app_app_config = mine_config.clone();
                                    let app_app_rpc_client = rpc_client.clone();
                                    let app_app_slack_message_sender = slack_message_sender.clone();
                                    let app_app_discord_message_sender =
                                        discord_message_sender.clone();
                                    let app_app_slack_difficulty = slack_difficulty.clone();
                                    let app_app_messaging_diff = messaging_diff.clone();
                                    let app_app_proof = app_proof.clone();
                                    let app_app_wallet = app_wallet.clone();
                                    let app_app_epoch_hashes = app_epoch_hashes.clone();
                                    // let app_app_no_sound_notification =
                                    //     app_no_sound_notification.clone();
                                    tokio::spawn(async move {
                                        let mine_success_sender = app_app_mine_success_sender;
                                        // let mission_completed_sender =
                                        //     mission_completed_sender;
                                        let app_nonce = app_app_nonce;
                                        let database = app_app_database;
                                        let mine_config = app_app_config;
                                        let rpc_client = app_app_rpc_client;
                                        let slack_message_sender =
                                            app_app_slack_message_sender.clone();
                                        let discord_message_sender =
                                            app_app_discord_message_sender.clone();
                                        let slack_difficulty = app_app_slack_difficulty.clone();
                                        let messaging_diff = app_app_messaging_diff.clone();
                                        let app_proof = app_app_proof;
                                        let app_wallet = app_app_wallet;
                                        let app_epoch_hashes = app_app_epoch_hashes;
                                        // let app_no_sound_notification =
                                        //     app_app_no_sound_notification;

                                        // if !*app_no_sound_notification {
                                        //     utils::play_sound();
                                        // }

                                        // update proof
                                        // limit number of checking no more than CHECK_LIMIT
                                        let mut num_checking = 0;
                                        loop {
                                            info!("Waiting & Checking for proof challenge update");
                                            // Wait 500ms then check for updated proof
                                            tokio::time::sleep(Duration::from_millis(500)).await;
                                            let lock = app_proof.lock().await;
                                            let latest_proof = lock.clone();
                                            drop(lock);

                                            if old_proof.challenge.eq(&latest_proof.challenge) {
                                                info!("Proof challenge not updated yet..");
                                                num_checking += 1;
                                                if num_checking >= CHECK_LIMIT {
                                                    warn!("No challenge update detected after {CHECK_LIMIT} checkpoints. No more waiting, just keep going...");
                                                    break;
                                                }
                                                // also check from rpc call along with proof
                                                // notification subscription
                                                if let Ok(p) = get_proof(
                                                    &rpc_client,
                                                    app_wallet.miner_wallet.pubkey(),
                                                )
                                                .await
                                                {
                                                    info!(
                                                        "OLD PROOF CHALLENGE: {}",
                                                        BASE64_STANDARD.encode(old_proof.challenge)
                                                    );
                                                    info!(
                                                        "RPC PROOF CHALLENGE: {}",
                                                        BASE64_STANDARD.encode(p.challenge)
                                                    );
                                                    if old_proof.challenge.ne(&p.challenge) {
                                                        info!("Found new proof from rpc call rather than websocket...");
                                                        let mut lock = app_proof.lock().await;
                                                        *lock = p;
                                                        drop(lock);

                                                        info!("Checking rewards earned.");
                                                        // let latest_proof =
                                                        //     { app_proof.lock().await.clone() };
                                                        let lock = app_proof.lock().await;
                                                        let latest_proof = lock.clone();
                                                        drop(lock);
                                                        let balance = (latest_proof.balance as f64)
                                                            / 10f64.powf(ORE_TOKEN_DECIMALS as f64);
                                                        info!("New balance: {}", balance);

                                                        let multiplier = if let Some(config) =
                                                            ore_config
                                                        {
                                                            if config.top_balance > 0 {
                                                                1.0 + (latest_proof.balance as f64
                                                                    / config.top_balance as f64)
                                                                    .min(1.0f64)
                                                            } else {
                                                                1.0f64
                                                            }
                                                        } else {
                                                            1.0f64
                                                        };
                                                        info!("Multiplier: {}", multiplier);

                                                        let rewards = (latest_proof.balance
                                                            - old_proof.balance)
                                                            as i64;
                                                        let dec_rewards = (rewards as f64)
                                                            / 10f64.powf(ORE_TOKEN_DECIMALS as f64);
                                                        info!("Earned: {} ORE", dec_rewards);

                                                        // reset nonce and epoch_hashes
                                                        info!("reset nonce and epoch hashes");
                                                        // reset nonce
                                                        {
                                                            let mut nonce = app_nonce.lock().await;
                                                            *nonce = 0;
                                                        }
                                                        // reset epoch hashes
                                                        {
                                                            let mut mut_epoch_hashes =
                                                                app_epoch_hashes.write().await;
                                                            mut_epoch_hashes.challenge =
                                                                p.challenge;
                                                            mut_epoch_hashes.best_hash.solution =
                                                                None;
                                                            mut_epoch_hashes.best_hash.difficulty =
                                                                0;
                                                            mut_epoch_hashes.contributions =
                                                                HashMap::new();
                                                        }

                                                        // unset mining pause flag to start new
                                                        // mining
                                                        // mission
                                                        info!("resume new mining mission");
                                                        PAUSED.store(false, Relaxed);

                                                        // Mission completed, send signal to tx
                                                        // sender
                                                        let _ = mission_completed_sender.send(0);

                                                        // Discussion: spawn a new task for database
                                                        // crud ???
                                                        if powered_by_dbms == &PoweredByDbms::Sqlite
                                                        {
                                                            // Add new challenge record to db if
                                                            // powered by dbms,
                                                            info!(
                                                                "Adding new challenge record to db"
                                                            );
                                                            let new_challenge =
                                                                models::InsertChallenge {
                                                                    pool_id: mine_config.pool_id,
                                                                    challenge: latest_proof
                                                                        .challenge
                                                                        .to_vec(),
                                                                    rewards_earned: None,
                                                                };

                                                            while let Err(_) = database
                                                                .add_new_challenge(
                                                                    new_challenge.clone(),
                                                                )
                                                                .await
                                                            {
                                                                error!("Failed to add new challenge record to db.");
                                                                info!("Check if the new challenge already exists in db.");
                                                                if let Ok(_) = database
                                                                    .get_challenge_by_challenge(
                                                                        new_challenge
                                                                            .challenge
                                                                            .clone(),
                                                                    )
                                                                    .await
                                                                {
                                                                    info!("Challenge already exists, continuing");
                                                                    break;
                                                                }

                                                                tokio::time::sleep(
                                                                    Duration::from_millis(1000),
                                                                )
                                                                .await;
                                                            }
                                                            info!("New challenge record successfully added to db");
                                                        }

                                                        break;
                                                    }
                                                } else {
                                                    error!("Failed to get proof via rpc call.");
                                                }
                                            } else {
                                                info!("Proof challenge updated!");
                                                let mut submission_challenge_id = i64::MAX;
                                                if powered_by_dbms == &PoweredByDbms::Sqlite {
                                                    // Fetch old proof challenge(id used later)
                                                    // records from db
                                                    info!("Check if old/last challenge for the pool exists in the database");
                                                    let old_challenge;
                                                    loop {
                                                        if let Ok(c) = database
                                                            .get_challenge_by_challenge(
                                                                old_proof.challenge.to_vec(),
                                                            )
                                                            .await
                                                        {
                                                            old_challenge = c;
                                                            submission_challenge_id =
                                                                old_challenge.id;
                                                            break;
                                                        } else {
                                                            warn!(
                                                                        "Failed to get old/last challenge record from db! Inserting if necessary..."
                                                                    );
                                                            let new_challenge =
                                                                models::InsertChallenge {
                                                                    pool_id: mine_config.pool_id,
                                                                    challenge: old_proof
                                                                        .challenge
                                                                        .to_vec(),
                                                                    rewards_earned: None,
                                                                };

                                                            while let Err(_) = database
                                                                .add_new_challenge(
                                                                    new_challenge.clone(),
                                                                )
                                                                .await
                                                            {
                                                                error!("Failed to add old/last challenge record to db.");
                                                                info!("Check if the challenge already exists in db.");
                                                                if let Ok(_) = database
                                                                    .get_challenge_by_challenge(
                                                                        new_challenge
                                                                            .challenge
                                                                            .clone(),
                                                                    )
                                                                    .await
                                                                {
                                                                    info!("The challenge already exists, continuing");
                                                                    break;
                                                                }

                                                                tokio::time::sleep(
                                                                    Duration::from_millis(1_000),
                                                                )
                                                                .await;
                                                            }
                                                            info!("Old/last challenge record successfully added to db");
                                                            // tokio::time::sleep(Duration::from_millis(1_000)).await;
                                                        }
                                                    }

                                                    // Add new challenge record to db
                                                    info!("Adding new challenge record to db");
                                                    let new_challenge = models::InsertChallenge {
                                                        pool_id: mine_config.pool_id,
                                                        challenge: latest_proof.challenge.to_vec(),
                                                        rewards_earned: None,
                                                    };

                                                    while let Err(_) = database
                                                        .add_new_challenge(new_challenge.clone())
                                                        .await
                                                    {
                                                        error!(
                                                                    "Failed to add new challenge record to db."
                                                                );
                                                        info!("Check if new challenge already exists in db.");
                                                        if let Ok(_) = database
                                                            .get_challenge_by_challenge(
                                                                new_challenge.challenge.clone(),
                                                            )
                                                            .await
                                                        {
                                                            info!(
                                                                        "Challenge already exists in db, continuing"
                                                                    );
                                                            break;
                                                        }

                                                        tokio::time::sleep(Duration::from_millis(
                                                            1_000,
                                                        ))
                                                        .await;
                                                    }
                                                    info!("New challenge record successfully added to db");
                                                }

                                                info!("Checking rewards earned.");
                                                // let latest_proof =
                                                //     { app_proof.lock().await.clone() };
                                                let lock = app_proof.lock().await;
                                                let latest_proof = lock.clone();
                                                drop(lock);
                                                let balance = (latest_proof.balance as f64)
                                                    / 10f64.powf(ORE_TOKEN_DECIMALS as f64);
                                                info!("New balance: {}", balance);

                                                let multiplier = if let Some(config) = ore_config {
                                                    if config.top_balance > 0 {
                                                        1.0 + (latest_proof.balance as f64
                                                            / config.top_balance as f64)
                                                            .min(1.0f64)
                                                    } else {
                                                        1.0f64
                                                    }
                                                } else {
                                                    1.0f64
                                                };
                                                info!("Multiplier: {}", multiplier);

                                                let rewards = (latest_proof.balance
                                                    - old_proof.balance)
                                                    as i64;
                                                let dec_rewards = (rewards as f64)
                                                    / 10f64.powf(ORE_TOKEN_DECIMALS as f64);
                                                info!("Earned: {} ORE", dec_rewards);

                                                let contributions = {
                                                    app_epoch_hashes
                                                        .read()
                                                        .await
                                                        .contributions
                                                        .clone()
                                                };

                                                let mut total_hashpower: u64 = 0;

                                                for contribution in contributions.iter() {
                                                    total_hashpower += contribution.1.hashpower;
                                                }

                                                let _ = mine_success_sender.send(
                                                    MessageInternalMineSuccess {
                                                        difficulty,
                                                        total_balance: balance,
                                                        rewards,
                                                        challenge_id: submission_challenge_id,
                                                        challenge: old_proof.challenge,
                                                        best_nonce: u64::from_le_bytes(
                                                            best_solution.n,
                                                        ),
                                                        total_hashpower,
                                                        ore_config,
                                                        multiplier,
                                                        contributions,
                                                    },
                                                );

                                                {
                                                    let mut mut_proof = app_proof.lock().await;
                                                    *mut_proof = latest_proof;
                                                    drop(mut_proof);
                                                }
                                                // reset nonce and epoch_hashes
                                                info!("reset nonce and epoch hashes");

                                                // reset nonce
                                                {
                                                    let mut nonce = app_nonce.lock().await;
                                                    *nonce = 0;
                                                }
                                                // reset epoch hashes
                                                {
                                                    let mut mut_epoch_hashes =
                                                        app_epoch_hashes.write().await;
                                                    mut_epoch_hashes.challenge =
                                                        latest_proof.challenge;
                                                    mut_epoch_hashes.best_hash.solution = None;
                                                    mut_epoch_hashes.best_hash.difficulty = 0;
                                                    mut_epoch_hashes.contributions = HashMap::new();
                                                }
                                                // unset mining pause flag to start new mining
                                                // mission
                                                info!("resume new mining mission");
                                                PAUSED.store(false, Relaxed);

                                                // Mission completed, send signal to tx sender
                                                let _ = mission_completed_sender.send(0);

                                                // last one, notify slack and other messaging
                                                // channels if necessary
                                                if difficulty.ge(&*slack_difficulty)
                                                    || difficulty.ge(&*messaging_diff)
                                                {
                                                    if messaging_flags
                                                        .contains(MessagingFlags::SLACK)
                                                    {
                                                        let _ = slack_message_sender.send(
                                                            RewardsMessage::Rewards(
                                                                difficulty,
                                                                dec_rewards,
                                                                balance,
                                                            ),
                                                        );
                                                    }

                                                    if messaging_flags
                                                        .contains(MessagingFlags::DISCORD)
                                                    {
                                                        let _ = discord_message_sender.send(
                                                            RewardsMessage::Rewards(
                                                                difficulty,
                                                                dec_rewards,
                                                                balance,
                                                            ),
                                                        );
                                                    }
                                                }

                                                break;
                                            }
                                        }
                                    });

                                    // MI: play sound here, meanwhile giving time to above spawned
                                    // task started
                                    if !*app_no_sound_notification {
                                        utils::play_sound();
                                    }

                                    if let Ok(_) = mission_completed_receiver.await {
                                        // all tx related activities have succeeded, exit tx submit
                                        // loop
                                        break;
                                    } else {
                                        error!(
                                            "Oops! No mission completion message yet. Waiting..."
                                        );
                                    }
                                }
                            } else {
                                error!("Solution is_some but got none on best hash re-check?");
                                tokio::time::sleep(Duration::from_millis(1_000)).await;
                            }
                        }
                        if !success {
                            error!("❌ Failed to land tx... either reached {SUBMIT_LIMIT} attempts or ix error or invalid solution.");
                            info!("Discarding and refreshing data...");
                            info!("refresh proof");
                            if let Ok(refreshed_proof) = get_proof(&rpc_client, wallet_pubkey).await
                            {
                                let mut app_proof = app_proof.lock().await;
                                *app_proof = refreshed_proof;
                                drop(app_proof);
                            }
                            info!("reset nonce and epoch hashes");
                            // reset nonce
                            {
                                let mut nonce = app_nonce.lock().await;
                                *nonce = 0;
                            }
                            // reset epoch hashes
                            {
                                let mut mut_epoch_hashes = app_epoch_hashes.write().await;
                                mut_epoch_hashes.best_hash.solution = None;
                                mut_epoch_hashes.best_hash.difficulty = 0;
                                mut_epoch_hashes.contributions = HashMap::new();
                            }

                            // unset mining pause flag to start new mining mission
                            info!("resume new mining mission");
                            PAUSED.store(false, Relaxed);
                        }
                        tokio::time::sleep(Duration::from_millis(500)).await;
                    } else {
                        // solution is None
                        // solution_is_none_counter += 1;
                        if solution_is_none_counter % NO_BEST_SOLUTION_INTERVAL == 0 {
                            info!("No best solution yet.");
                        }
                        solution_is_none_counter += 1;
                        tokio::time::sleep(Duration::from_millis(1_000)).await;
                    }
                }
            } else {
                // cutoff > 0
                // reset none solution counter
                solution_is_none_counter = 0;
                info!("Cutoff countdown(every 5 seconds): {}s", cutoff);
                // println!("Cutoff countdown: {}s", cutoff);
                // make sure to sleep between 1..=5 seconds
                tokio::time::sleep(Duration::from_secs(cutoff.min(5).max(1) as u64)).await;
            };
        }
    });

    let app_shared_state = shared_state.clone();
    let app_database = database.clone();
    let app_rpc_client = rpc_client.clone();
    let app_wallet = wallet_extension.clone();
    tokio::spawn(async move {
        let database = app_database;
        loop {
            let mut sol_balance_checking = 0_u64;
            while let Some(msg) = mine_success_receiver.recv().await {
                if powered_by_dbms == &PoweredByDbms::Sqlite {
                    let mut i_earnings = Vec::new();
                    let mut i_rewards = Vec::new();
                    let mut i_contributions = Vec::new();

                    for (miner_pubkey, msg_contribution) in msg.contributions.iter() {
                        let hashpower_percent = (msg_contribution.hashpower as u128)
                            .saturating_mul(1_000_000)
                            .saturating_div(msg.total_hashpower as u128);

                        // TODO: handle overflow/underflow and float imprecision issues
                        let decimals = 10f64.powf(ORE_TOKEN_DECIMALS as f64);
                        let earned_rewards = hashpower_percent
                            .saturating_mul(msg.rewards as u128)
                            .saturating_div(1_000_000)
                            as i64;

                        let new_earning = InsertEarning {
                            miner_id: msg_contribution.miner_id,
                            pool_id: mine_config.pool_id,
                            challenge_id: msg.challenge_id,
                            amount: earned_rewards,
                        };

                        let new_contribution = InsertContribution {
                            miner_id: msg_contribution.miner_id,
                            challenge_id: msg.challenge_id,
                            nonce: msg_contribution.supplied_nonce,
                            difficulty: msg_contribution.supplied_diff as i16,
                        };

                        let new_reward = UpdateReward {
                            miner_id: msg_contribution.miner_id,
                            balance: earned_rewards,
                        };

                        i_earnings.push(new_earning);
                        i_rewards.push(new_reward);
                        i_contributions.push(new_contribution);

                        let earned_rewards_dec = (earned_rewards as f64).div(decimals);
                        let pool_rewards_dec = (msg.rewards as f64).div(decimals);

                        let percentage = if pool_rewards_dec != 0.0 {
                            (earned_rewards_dec / pool_rewards_dec) * 100.0
                        } else {
                            0.0 // Handle the case where pool_rewards_dec is 0 to avoid division by
                                // zero
                        };

                        let top_stake = if let Some(config) = msg.ore_config {
                            (config.top_balance as f64).div(decimals)
                        } else {
                            1.0f64
                        };

                        let shared_state = app_shared_state.read().await;
                        let len = shared_state.sockets.len();
                        let message = format!(
                            "Pool Submitted Difficulty: {}\nPool Earned:  {:.11} ORE\nPool Balance: {:.11} ORE\nTop Stake:    {:.11} ORE\nPool Multiplier: {:.2}x\n----------------------\nActive Miners: {}\n----------------------\nMiner Submitted Difficulty: {}\nMiner Earned: {:.11} ORE\n{:.2}% of total pool reward",
                            msg.difficulty,
                            pool_rewards_dec,
                            msg.total_balance,
                            top_stake,
                            msg.multiplier,
                            len,
                            msg_contribution.supplied_diff,
                            earned_rewards_dec,
                            percentage
                        );

                        for (_addr, client_connection) in shared_state.sockets.iter() {
                            let client_message = message.clone();
                            if client_connection.pubkey.eq(&miner_pubkey) {
                                let socket_sender = client_connection.socket.clone();
                                tokio::spawn(async move {
                                    if let Ok(_) = socket_sender
                                        .lock()
                                        .await
                                        .send(Message::Text(client_message))
                                        .await
                                    {
                                    } else {
                                        error!("Failed to send client text");
                                    }
                                });
                            }
                        }
                    }

                    if i_earnings.len() > 0 {
                        if let Ok(_) = database.add_new_earnings_batch(i_earnings.clone()).await {
                            info!("Successfully added earnings batch");
                        } else {
                            error!("Failed to insert earnings batch");
                        }
                    }
                    if i_rewards.len() > 0 {
                        if let Ok(_) = database.update_rewards(i_rewards).await {
                            info!("Successfully updated rewards");
                        } else {
                            error!("Failed to bulk update rewards");
                        }
                    }
                    if i_contributions.len() > 0 {
                        if let Ok(_) = database.add_new_contributions_batch(i_contributions).await {
                            info!("Successfully added contributions batch");
                        } else {
                            error!("Failed to insert contributions batch");
                        }
                    }
                    while let Err(_) = database
                        .update_pool_rewards(
                            app_wallet.miner_wallet.pubkey().to_string(),
                            msg.rewards,
                        )
                        .await
                    {
                        error!("Failed to update pool rewards! Retrying...");
                        tokio::time::sleep(Duration::from_millis(1000)).await;
                    }

                    tokio::time::sleep(Duration::from_millis(200)).await;
                    let contribution_id;
                    loop {
                        if let Ok(cid) =
                            database.get_contribution_id_with_nonce(msg.best_nonce).await
                        {
                            contribution_id = cid;
                            break;
                        } else {
                            error!("Failed to get contribution id with nonce! Retrying...");
                            tokio::time::sleep(Duration::from_millis(1000)).await;
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    if let Err(_) = database
                        .update_challenge_rewards(
                            msg.challenge.to_vec(),
                            contribution_id,
                            msg.rewards,
                        )
                        .await
                    {
                        error!("Failed to update challenge rewards! Skipping! Devs check!");
                        let err_str = format!("Challenge UPDATE FAILED - Challenge: {:?}\nContribution ID: {}\nRewards: {}\n", msg.challenge.to_vec(), contribution_id, msg.rewards);
                        error!(err_str);
                    }
                } else {
                    let decimals = 10f64.powf(ORE_TOKEN_DECIMALS as f64);
                    let pool_rewards_dec = (msg.rewards as f64).div(decimals);
                    let shared_state = app_shared_state.read().await;
                    let len = shared_state.sockets.len();
                    for (_socket_addr, socket_sender) in shared_state.sockets.iter() {
                        let pubkey = socket_sender.pubkey;

                        if let Some(InternalMessageContribution {
                            miner_id: _,
                            supplied_diff,
                            supplied_nonce: _,
                            hashpower: pubkey_hashpower,
                        }) = msg.contributions.get(&pubkey)
                        {
                            let hashpower_percent = (*pubkey_hashpower as u128)
                                .saturating_mul(1_000_000)
                                .saturating_div(msg.total_hashpower as u128);

                            let earned_rewards = hashpower_percent
                                .saturating_mul(msg.rewards as u128)
                                .saturating_div(1_000_000)
                                as u64;

                            let earned_rewards_dec = (earned_rewards as f64).div(decimals);

                            let percentage = if pool_rewards_dec != 0.0 {
                                (earned_rewards_dec / pool_rewards_dec) * 100.0
                            } else {
                                0.0 // Handle the case where pool_rewards_dec is 0 to avoid division
                                    // by zero
                            };

                            let message = format!(
                                "Pool Submitted Difficulty: {}\nPool Earned:  {:.11} ORE\nPool Balance: {:.11} ORE\n----------------------\nActive Miners: {}\n----------------------\nMiner Submitted Difficulty: {}\nMiner Earned: {:.11} ORE\n{:.2}% of total pool reward",
                                msg.difficulty,
                                pool_rewards_dec,
                                msg.total_balance,
                                len,
                                supplied_diff,
                                earned_rewards_dec,
                                percentage
                            );

                            let socket_sender = socket_sender.clone();
                            tokio::spawn(async move {
                                if let Ok(_) = socket_sender
                                    .socket
                                    .lock()
                                    .await
                                    .send(Message::Text(message))
                                    .await
                                {
                                } else {
                                    error!("Failed to send client text");
                                }
                            });
                        }
                    }
                }

                if sol_balance_checking % 10 == 0 {
                    if let Ok(balance) =
                        app_rpc_client.get_balance(&app_wallet.miner_wallet.pubkey()).await
                    {
                        info!(
                            "Sol Balance(of miner wallet): {:.9}",
                            balance as f64 / LAMPORTS_PER_SOL as f64
                        );
                    } else {
                        error!("Failed to load balance");
                    }
                }
                sol_balance_checking += 1;
            }
        }
    });

    let app_shared_state = shared_state.clone();
    tokio::spawn(async move {
        loop {
            while let Some(msg) = all_clients_receiver.recv().await {
                {
                    let shared_state = app_shared_state.read().await;
                    for (_socket_addr, socket_sender) in shared_state.sockets.iter() {
                        let text = msg.text.clone();
                        let socket = socket_sender.clone();
                        tokio::spawn(async move {
                            if let Ok(_) =
                                socket.socket.lock().await.send(Message::Text(text)).await
                            {
                            } else {
                                error!("Failed to send client text");
                            }
                        });
                    }
                }
            }
        }
    });

    let cors = CorsLayer::new().allow_methods([Method::GET]).allow_origin(tower_http::cors::Any);

    let client_channel = client_message_sender.clone();
    let app_shared_state = shared_state.clone();
    let app = Router::new()
        .route("/", get(ws_handler))
        .route("/latest-blockhash", get(get_latest_blockhash))
        .route("/pool/authority/pubkey", get(get_pool_authority_pubkey))
        .with_state(app_shared_state)
        .layer(Extension(database))
        .layer(Extension(wallet_extension))
        .layer(Extension(client_channel))
        .layer(Extension(rpc_client))
        .layer(Extension(client_nonce_ranges))
        // Logging
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::default().include_headers(true)),
        )
        .layer(cors);

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();

    tracing::info!("listening on {}", listener.local_addr().unwrap());

    let app_shared_state = shared_state.clone();
    tokio::spawn(async move {
        ping_check_system(&app_shared_state).await;
    });

    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await.unwrap();

    Ok(())
}

async fn get_pool_authority_pubkey(
    Extension(wallet): Extension<Arc<WalletExtension>>,
) -> impl IntoResponse {
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "text/text")
        .body(wallet.miner_wallet.pubkey().to_string())
        .unwrap()
}

async fn _get_pool_fee_payer_pubkey(
    Extension(wallet): Extension<Arc<WalletExtension>>,
) -> impl IntoResponse {
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "text/text")
        .body(wallet.fee_wallet.pubkey().to_string())
        .unwrap()
}

async fn get_latest_blockhash(
    Extension(rpc_client): Extension<Arc<RpcClient>>,
) -> impl IntoResponse {
    let latest_blockhash = rpc_client.get_latest_blockhash().await.unwrap();

    let serialized_blockhash = bincode::serialize(&latest_blockhash).unwrap();

    let encoded_blockhash = BASE64_STANDARD.encode(serialized_blockhash);
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "text/text")
        .body(encoded_blockhash)
        .unwrap()
}

#[derive(Deserialize)]
struct WsQueryParams {
    timestamp: u64,
}

#[debug_handler]
async fn ws_handler(
    ws: WebSocketUpgrade,
    TypedHeader(auth_header): TypedHeader<axum_extra::headers::Authorization<Basic>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(app_state): State<Arc<RwLock<AppState>>>,
    // Extension(mine_config): Extension<Arc<MineConfig>>,
    Extension(client_channel): Extension<UnboundedSender<ClientMessage>>,
    Extension(database): Extension<Arc<Database>>,
    query_params: Query<WsQueryParams>,
) -> impl IntoResponse {
    let msg_timestamp = query_params.timestamp;

    let pubkey = auth_header.username();
    let signed_msg = auth_header.password();

    let now = SystemTime::now().duration_since(UNIX_EPOCH).expect("Time went backwards").as_secs();

    // Signed authentication message is only valid for 30 seconds
    if (now - query_params.timestamp) >= 30 {
        return Err((StatusCode::UNAUTHORIZED, "Timestamp too old."));
    }

    let powered_by_dbms = POWERED_BY_DBMS.get_or_init(|| {
        // let mut powered_by_dbms = PoweredByDbms::Unavailable;
        let key = "POWERED_BY_DBMS";
        match std::env::var(key) {
            Ok(val) =>
                PoweredByDbms::from_str(&val).expect("POWERED_BY_DBMS must be set correctly."),
            Err(_) => PoweredByDbms::Unavailable,
        }
    });

    // verify client
    if let Ok(user_pubkey) = Pubkey::from_str(pubkey) {
        if powered_by_dbms == &PoweredByDbms::Sqlite {
            info!("Check if the miner record exists in the database");
            let db_miner = database.get_miner_by_pubkey_str(pubkey.to_string()).await;

            let miner;
            match db_miner {
                Ok(db_miner) => {
                    miner = db_miner;
                },
                // Err(DatabaseError::QueryFailed) => {
                //     return Err((
                //         StatusCode::UNAUTHORIZED,
                //         "pubkey is not authorized to mine. please sign up.",
                //     ));
                // },
                // Err(DatabaseError::InteractionFailed) => {
                //     return Err((
                //         StatusCode::UNAUTHORIZED,
                //         "pubkey is not authorized to mine. please sign up.",
                //     ));
                // },
                Err(DatabaseError::QueryFailed) | Err(DatabaseError::InteractionFailed) => {
                    info!("Miner pubkey record missing from database. Inserting...");
                    let result = database
                        .add_new_miner(user_pubkey.to_string(), true, "Enrolled".to_string())
                        .await;
                    miner =
                        database.get_miner_by_pubkey_str(user_pubkey.to_string()).await.unwrap();

                    let wallet_pubkey = user_pubkey;
                    let pool = database
                        .get_pool_by_authority_pubkey(wallet_pubkey.to_string())
                        .await
                        .unwrap();

                    if result.is_ok() {
                        let new_reward = InsertReward { miner_id: miner.id, pool_id: pool.id };
                        let result = database.add_new_reward(new_reward).await;

                        if result.is_ok() {
                            info!("Miner and rewards tracker added to database");
                        } else {
                            error!("Failed to add miner rewards tracker to database");
                            return Err((
                                StatusCode::UNAUTHORIZED,
                                "Failed to add miner rewards tracker to database",
                            ));
                        }
                        info!("Miner record added to database");
                    } else {
                        error!("Failed to add miner record to database");
                        return Err((
                            StatusCode::UNAUTHORIZED,
                            "Failed to add miner record to database",
                        ));
                    }
                },
                Err(DatabaseError::FailedToGetConnectionFromPool) => {
                    error!("Failed to get database pool connection.");
                    return Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error"));
                },
                Err(_) => {
                    error!("DB Error: Catch all.");
                    return Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error"));
                },
            }

            if !miner.enabled {
                return Err((StatusCode::UNAUTHORIZED, "pubkey is not authorized to mine"));
            }

            if let Ok(signature) = Signature::from_str(signed_msg) {
                let ts_msg = msg_timestamp.to_le_bytes();

                if signature.verify(&user_pubkey.to_bytes(), &ts_msg) {
                    info!("Client: {addr} connected with pubkey {pubkey}.");
                    return Ok(ws.on_upgrade(move |socket| {
                        handle_socket(
                            socket,
                            addr,
                            user_pubkey,
                            miner.id,
                            app_state,
                            client_channel,
                        )
                    }));
                } else {
                    return Err((StatusCode::UNAUTHORIZED, "Sig verification failed"));
                }
            } else {
                return Err((StatusCode::UNAUTHORIZED, "Invalid signature"));
            }
        } else {
            {
                let mut already_connected = false;
                for (_, client_connection) in app_state.read().await.sockets.iter() {
                    if user_pubkey == client_connection.pubkey {
                        already_connected = true;
                        break;
                    }
                }
                if already_connected {
                    return Err((
                        StatusCode::TOO_MANY_REQUESTS,
                        "A client is already connected with that wallet",
                    ));
                }
            };

            if let Ok(signature) = Signature::from_str(signed_msg) {
                let ts_msg = msg_timestamp.to_le_bytes();

                if signature.verify(&user_pubkey.to_bytes(), &ts_msg) {
                    info!("Client: {addr} connected with pubkey {pubkey}.");
                    return Ok(ws.on_upgrade(move |socket| {
                        handle_socket(
                            socket,
                            addr,
                            user_pubkey,
                            // MI: default miner_id for non-dbms
                            i64::MAX,
                            app_state,
                            client_channel,
                        )
                    }));
                } else {
                    return Err((StatusCode::UNAUTHORIZED, "Sig verification failed"));
                }
            } else {
                return Err((StatusCode::UNAUTHORIZED, "Invalid signature"));
            }
        }
    } else {
        return Err((StatusCode::UNAUTHORIZED, "Invalid pubkey"));
    }
}

async fn handle_socket(
    mut socket: WebSocket,
    who: SocketAddr,
    who_pubkey: Pubkey,
    who_miner_id: i64,
    rw_app_state: Arc<RwLock<AppState>>,
    client_channel: UnboundedSender<ClientMessage>,
) {
    if socket.send(axum::extract::ws::Message::Ping(vec![1, 2, 3])).await.is_ok() {
        tracing::debug!("Pinged {who}... pubkey: {who_pubkey}");
    } else {
        error!("could not ping {who} pubkey: {who_pubkey}");

        // if we can't ping we can't do anything, return to close the connection
        return;
    }

    let (sender, mut receiver) = socket.split();
    let mut app_state = rw_app_state.write().await;
    if app_state.sockets.contains_key(&who) {
        info!("Socket addr: {who} already has an active connection");
        return;
    } else {
        let new_client_connection = ClientConnection {
            pubkey: who_pubkey,
            miner_id: who_miner_id,
            socket: Arc::new(Mutex::new(sender)),
        };
        app_state.sockets.insert(who, new_client_connection);
    }
    drop(app_state);

    let _ = tokio::spawn(async move {
        // MI: vanilla. by design while let will exit when None received
        while let Some(Ok(msg)) = receiver.next().await {
            if process_message(msg, who, client_channel.clone()).is_break() {
                break;
            }
        }

        // // MI: use loop, since by design while let will exit when None received
        // loop {
        //     if let Some(Ok(msg)) = receiver.next().await {
        //         if process_message(msg, who, client_channel.clone()).is_break() {
        //             break;
        //         }
        //     }
        // }
    })
    .await;

    let mut app_state = rw_app_state.write().await;
    app_state.sockets.remove(&who);
    drop(app_state);

    info!("Client: {} disconnected!", who_pubkey.to_string());
}

fn process_message(
    msg: Message,
    who: SocketAddr,
    client_channel: UnboundedSender<ClientMessage>,
) -> ControlFlow<(), ()> {
    match msg {
        Message::Text(_t) => {
            // info!(">>> {who} sent str: {t:?}");
        },
        Message::Binary(d) => {
            // first 8 bytes are message type
            let message_type = d[0];
            match message_type {
                0 => {
                    let msg = ClientMessage::Ready(who);
                    let _ = client_channel.send(msg);
                },
                1 => {
                    let msg = ClientMessage::Mining(who);
                    let _ = client_channel.send(msg);
                },
                2 => {
                    // parse solution from message data
                    let mut solution_bytes = [0u8; 16];
                    // extract (16 u8's) from data for hash digest
                    let mut b_index = 1;
                    for i in 0..16 {
                        solution_bytes[i] = d[i + b_index];
                    }
                    b_index += 16;

                    // extract 64 bytes (8 u8's)
                    let mut nonce = [0u8; 8];
                    for i in 0..8 {
                        nonce[i] = d[i + b_index];
                    }
                    b_index += 8;

                    let mut pubkey = [0u8; 32];
                    for i in 0..32 {
                        pubkey[i] = d[i + b_index];
                    }

                    b_index += 32;

                    let signature_bytes = d[b_index..].to_vec();
                    if let Ok(sig_str) = String::from_utf8(signature_bytes.clone()) {
                        if let Ok(sig) = Signature::from_str(&sig_str) {
                            let pubkey = Pubkey::new_from_array(pubkey);

                            let mut hash_nonce_message = [0; 24];
                            hash_nonce_message[0..16].copy_from_slice(&solution_bytes);
                            hash_nonce_message[16..24].copy_from_slice(&nonce);

                            if sig.verify(&pubkey.to_bytes(), &hash_nonce_message) {
                                let solution = Solution::new(solution_bytes, nonce);

                                let msg = ClientMessage::BestSolution(who, solution, pubkey);
                                let _ = client_channel.send(msg);
                            } else {
                                error!("Client contribution sig verification failed.");
                            }
                        } else {
                            error!("Failed to parse into Signature.");
                        }
                    } else {
                        error!("Failed to parse signed message from client.");
                    }
                },
                _ => {
                    error!(">>> {} sent an invalid message", who);
                },
            }
        },
        Message::Close(c) => {
            if let Some(cf) = c {
                info!(">>> {} sent close with code {} and reason `{}`", who, cf.code, cf.reason);
            } else {
                info!(">>> {who} somehow sent close message without CloseFrame");
            }
            return ControlFlow::Break(());
        },
        Message::Pong(_v) => {
            let msg = ClientMessage::Pong(who);
            let _ = client_channel.send(msg);
        },
        Message::Ping(_v) => {
            //info!(">>> {who} sent ping with {v:?}");
        },
    }

    ControlFlow::Continue(())
}

async fn proof_tracking_system(ws_url: String, wallet: Arc<Keypair>, proof: Arc<Mutex<Proof>>) {
    loop {
        info!("Establishing rpc websocket connection...");
        let mut ps_client = PubsubClient::new(&ws_url).await;

        while ps_client.is_err() {
            error!("Failed to connect to websocket, retrying...");
            ps_client = PubsubClient::new(&ws_url).await;
            tokio::time::sleep(Duration::from_millis(1000)).await;
        }
        info!("RPC WS connection established!");

        let app_wallet = wallet.clone();
        if let Ok(ps_client) = ps_client {
            // The `PubsubClient` must be `Arc`ed to share it across threads/tasks.
            let ps_client = Arc::new(ps_client);
            let app_proof = proof.clone();
            let account_pubkey = proof_pubkey(app_wallet.pubkey());
            let ps_client = Arc::clone(&ps_client); // MI
            let pubsub = ps_client
                .account_subscribe(
                    &account_pubkey,
                    Some(RpcAccountInfoConfig {
                        encoding: Some(UiAccountEncoding::Base64),
                        data_slice: None,
                        commitment: Some(CommitmentConfig::confirmed()),
                        min_context_slot: None,
                    }),
                )
                .await;

            info!("Subscribed notification of pool proof updates via websocket");
            if let Ok((mut account_sub_notifications, _account_unsub)) = pubsub {
                // MI: vanilla, by design while let will exit when None received
                while let Some(response) = account_sub_notifications.next().await {
                    let data = response.value.data.decode();
                    if let Some(data_bytes) = data {
                        if let Ok(new_proof) = Proof::try_from_bytes(&data_bytes) {
                            info!(
                                "Received new proof with challenge: {}",
                                BASE64_STANDARD.encode(new_proof.challenge)
                            );
                            {
                                let mut app_proof = app_proof.lock().await;
                                *app_proof = *new_proof;
                                drop(app_proof);
                            }
                        }
                    }
                }

                // // MI: use loop, by design while let will exit when None received
                // loop {
                //     if let Some(response) = account_sub_notifications.next().await {
                //         let data = response.value.data.decode();
                //         if let Some(data_bytes) = data {
                //             if let Ok(new_proof) = Proof::try_from_bytes(&data_bytes) {
                //                 {
                //                     let mut app_proof = app_proof.lock().await;
                //                     *app_proof = *new_proof;
                //                     drop(app_proof);
                //                 }
                //             }
                //         }
                //     }
                // }
            }
        }
    }
}

async fn pong_tracking_system(app_pongs: Arc<RwLock<LastPong>>, app_state: Arc<RwLock<AppState>>) {
    loop {
        let reader = app_pongs.read().await;
        let pongs = reader.pongs.clone();
        drop(reader);

        for pong in pongs.iter() {
            if pong.1.elapsed().as_secs() > 45 {
                let mut writer = app_state.write().await;
                writer.sockets.remove(pong.0);
                drop(writer);

                let mut writer = app_pongs.write().await;
                writer.pongs.remove(pong.0);
                drop(writer)
            }
        }

        tokio::time::sleep(Duration::from_secs(15)).await;
    }
}

async fn client_message_handler_system(
    app_state: Arc<RwLock<AppState>>,
    mut receiver_channel: UnboundedReceiver<ClientMessage>,
    epoch_hashes: Arc<RwLock<EpochHashes>>,
    ready_clients: Arc<Mutex<HashSet<SocketAddr>>>,
    proof: Arc<Mutex<Proof>>,
    client_nonce_ranges: Arc<RwLock<HashMap<Pubkey, Range<u64>>>>,
    app_pongs: Arc<RwLock<LastPong>>,
    min_difficulty: u32,
) {
    loop {
        if let Some(client_message) = receiver_channel.recv().await {
            match client_message {
                ClientMessage::Pong(addr) => {
                    let mut writer = app_pongs.write().await;
                    writer.pongs.insert(addr, Instant::now());
                    drop(writer);
                },
                ClientMessage::Ready(addr) => {
                    let ready_clients = ready_clients.clone();
                    tokio::spawn(async move {
                        // info!("Client {} is ready!", addr.to_string());
                        let mut ready_clients = ready_clients.lock().await;
                        ready_clients.insert(addr);
                    });
                },
                ClientMessage::Mining(addr) => {
                    info!("Client {} has started mining!", addr.to_string());
                },
                ClientMessage::BestSolution(addr, solution, pubkey) => {
                    let app_epoch_hashes = epoch_hashes.clone();
                    let app_proof = proof.clone();
                    let app_client_nonce_ranges = client_nonce_ranges.clone();
                    let app_state = app_state.clone();
                    tokio::spawn(async move {
                        let epoch_hashes = app_epoch_hashes;
                        let proof = app_proof;
                        let client_nonce_ranges = app_client_nonce_ranges;

                        let pubkey_str = pubkey.to_string();
                        let len = pubkey_str.len();
                        let short_pbukey_str =
                            format!("{}...{}", &pubkey_str[0..6], &pubkey_str[len - 4..len]);

                        let reader = client_nonce_ranges.read().await;
                        let nonce_range: Range<u64> = {
                            if let Some(nr) = reader.get(&pubkey) {
                                nr.clone()
                            } else {
                                error!("Client nonce range not set!");
                                return;
                            }
                        };
                        drop(reader);

                        let nonce = u64::from_le_bytes(solution.n);

                        if !nonce_range.contains(&nonce) {
                            error!("❌ Client submitted nonce out of assigned range");
                            return;
                        }

                        let reader = app_state.read().await;
                        let miner_id;
                        if let Some(app_client_socket) = reader.sockets.get(&addr) {
                            miner_id = app_client_socket.miner_id;
                        } else {
                            error!("Failed to get client socket for addr: {}", addr);
                            return;
                        }
                        drop(reader);

                        let lock = proof.lock().await;
                        let challenge = lock.challenge;
                        drop(lock);
                        if solution.is_valid(&challenge) {
                            let diff = solution.to_hash().difficulty();
                            info!(
                                "{} found diff: {} at {}",
                                // pubkey_str,
                                short_pbukey_str,
                                diff,
                                Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
                            );
                            // if diff >= MIN_DIFF {
                            if diff >= min_difficulty {
                                // calculate rewards, only diff larger than min_difficulty(rather
                                // than MIN_DIFF) qualifies rewards calc.
                                let mut hashpower = MIN_HASHPOWER * 2u64.pow(diff - MIN_DIFF);
                                if hashpower > 81_920 {
                                    hashpower = 81_920;
                                }
                                {
                                    let reader = epoch_hashes.read().await;
                                    let subs = reader.contributions.clone();
                                    drop(reader);

                                    if let Some(old_sub) = subs.get(&pubkey) {
                                        if diff > old_sub.supplied_diff {
                                            let mut epoch_hashes = epoch_hashes.write().await;
                                            epoch_hashes.contributions.insert(
                                                pubkey,
                                                InternalMessageContribution {
                                                    miner_id,
                                                    supplied_nonce: nonce,
                                                    supplied_diff: diff,
                                                    hashpower,
                                                },
                                            );
                                            if diff > epoch_hashes.best_hash.difficulty {
                                                info!("New best diff: {}", diff);
                                                epoch_hashes.best_hash.difficulty = diff;
                                                epoch_hashes.best_hash.solution = Some(solution);
                                            }
                                            drop(epoch_hashes);
                                        } else {
                                            info!("Miner submitted lower diff than a previous contribution, discarding lower diff");
                                        }
                                    } else {
                                        let mut epoch_hashes = epoch_hashes.write().await;
                                        epoch_hashes.contributions.insert(
                                            pubkey,
                                            InternalMessageContribution {
                                                miner_id,
                                                supplied_nonce: nonce,
                                                supplied_diff: diff,
                                                hashpower,
                                            },
                                        );
                                        if diff > epoch_hashes.best_hash.difficulty {
                                            info!("New best diff: {}", diff);
                                            epoch_hashes.best_hash.difficulty = diff;
                                            epoch_hashes.best_hash.solution = Some(solution);
                                        }
                                        drop(epoch_hashes);
                                    }
                                }
                                // tokio::time::sleep(Duration::from_millis(100)).await;
                            } else {
                                warn!("Diff too low, skipping");
                            }
                        } else {
                            error!(
                                "{} returned an invalid solution for latest challenge!",
                                // pubkey
                                short_pbukey_str
                            );

                            let reader = app_state.read().await;
                            if let Some(app_client_socket) = reader.sockets.get(&addr) {
                                let _ = app_client_socket.socket.lock().await.send(Message::Text("Invalid solution. If this keeps happening, please contact support.".to_string())).await;
                            } else {
                                error!("Failed to get client socket for addr: {}", addr);
                                return;
                            }
                            drop(reader);
                        }
                    });
                },
            }
        }
    }
}

async fn ping_check_system(shared_state: &Arc<RwLock<AppState>>) {
    loop {
        // send ping to all sockets
        let app_state = shared_state.read().await;

        let mut handles = Vec::new();
        for (who, socket) in app_state.sockets.iter() {
            let who = who.clone();
            let socket = socket.clone();
            handles.push(tokio::spawn(async move {
                if socket.socket.lock().await.send(Message::Ping(vec![1, 2, 3])).await.is_ok() {
                    return None;
                } else {
                    return Some(who.clone());
                }
            }));
        }
        drop(app_state);

        // remove any sockets where ping failed
        for handle in handles {
            match handle.await {
                Ok(Some(who)) => {
                    let mut app_state = shared_state.write().await;
                    app_state.sockets.remove(&who);
                },
                Ok(None) => {},
                Err(_) => {
                    error!("Got error sending ping to client.");
                },
            }
        }

        tokio::time::sleep(Duration::from_secs(30)).await;
    }
}

async fn reporting_system(
    interval_in_hrs: u64,
    mine_config: Arc<MineConfig>,
    database: Arc<Database>,
) {
    // initial report starts in 5 mins(300s)
    let mut time_to_next_reporting: u64 = 300;
    let mut timer = Instant::now();
    loop {
        let current_timestamp = timer.elapsed().as_secs();
        if current_timestamp.ge(&time_to_next_reporting) {
            if POWERED_BY_DBMS.get() == Some(&PoweredByDbms::Sqlite) {
                info!("Preparing client summaries for last 24 hours.");
                let summaries_last_24_hrs =
                    database.get_summaries_for_last_24_hours(mine_config.pool_id).await;

                match summaries_last_24_hrs {
                    Ok(summaries) => {
                        // printing report header
                        let report_title = "Miner summaries for last 24 hours:";
                        info!("{report_title}");
                        println!("{report_title}");
                        let line_header = format!(
                            "miner_pubkey     num_of_qualified_contributions   min_diff   avg_diff   max_diff   earning_sub_total   percent"
                        );
                        info!("{line_header}");
                        println!("{line_header}");

                        let decimals = 10f64.powf(ORE_TOKEN_DECIMALS as f64);
                        for summary in summaries {
                            let mp = summary.miner_pubkey;
                            let len = mp.len();
                            let short_mp = format!("{}...{}", &mp[0..6], &mp[len - 4..len]);
                            let earned_rewards_dec =
                                (summary.earning_sub_total as f64).div(decimals);
                            let line = format!(
                                "{}    {:30}   {:8}   {:8}   {:8}       {:.11}   {}",
                                short_mp,
                                summary.num_of_contributions,
                                summary.min_diff,
                                summary.avg_diff,
                                summary.max_diff,
                                earned_rewards_dec,
                                summary.percent
                            );

                            info!("{line}");
                            println!("{line}");
                        }
                    },
                    Err(e) => {
                        error!("Failed to prepare summary report: {e:?}");
                    },
                }
                time_to_next_reporting = interval_in_hrs * 3600; // in seconds
                timer = Instant::now();
            }
        } else {
            tokio::time::sleep(Duration::from_secs(
                time_to_next_reporting.saturating_sub(current_timestamp),
            ))
            .await;
        }
    }
}

fn styles() -> Styles {
    Styles::styled()
        .header(AnsiColor::Red.on_default() | Effects::BOLD)
        .usage(AnsiColor::Red.on_default() | Effects::BOLD)
        .literal(AnsiColor::Blue.on_default() | Effects::BOLD)
        .placeholder(AnsiColor::Green.on_default())
}
