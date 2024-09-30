use {
    self::models::*,
    // ::ore_utils::AccountDeserialize,
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
    futures::{stream::SplitSink, StreamExt},
    notification::RewardsMessage,
    ore_api::consts::EPOCH_DURATION,
    processors::{
        client_message_processor::client_message_processor,
        messaging_all_clients_processor::messaging_all_clients_processor,
        ping_check_processor::ping_check_processor,
        pong_tracking_processor::pong_tracking_processor,
        pool_mine_success_processor::pool_mine_success_processor,
        pool_submission_processor::pool_submission_processor,
        proof_tracking_processor::proof_tracking_processor,
        ready_clients_processor::ready_clients_processor, reporting_processor::reporting_processor,
    },
    rusqlite::Connection,
    serde::Deserialize,
    solana_client::nonblocking::rpc_client::RpcClient,
    solana_sdk::{
        commitment_config::CommitmentConfig,
        native_token::LAMPORTS_PER_SOL,
        pubkey::Pubkey,
        signature::{read_keypair_file, Keypair, Signature},
        signer::Signer,
        transaction::Transaction,
    },
    std::{
        collections::{HashMap, HashSet},
        net::SocketAddr,
        ops::ControlFlow,
        path::Path,
        str::FromStr,
        sync::{atomic::AtomicBool, Arc, Once, OnceLock},
        time::{SystemTime, UNIX_EPOCH},
    },
    tokio::{
        sync::{mpsc::UnboundedSender, Mutex, RwLock},
        time::Instant,
    },
    tower_http::{
        cors::CorsLayer,
        trace::{DefaultMakeSpan, TraceLayer},
    },
    tracing::{debug, error, info, warn},
    tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, Layer},
    utils::{get_proof, get_register_ix, proof_pubkey},
};

mod database;
mod dynamic_fee;
mod message;
mod models;
mod notification;
mod processors;
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
static WALLET_PUBKEY: OnceLock<Pubkey> = OnceLock::new();
static PAUSED: AtomicBool = AtomicBool::new(false);

static mut MESSAGING_FLAGS: MessagingFlags = MessagingFlags::empty();
static INIT_MESSAGING_FLAGS: Once = Once::new();

static mut SLACK_WEBHOOK: String = String::new();
static mut DISCORD_WEBHOOK: String = String::new();

fn get_messaging_flags() -> MessagingFlags {
    unsafe {
        INIT_MESSAGING_FLAGS.call_once(|| {
            let mut exists_slack_webhook = false;
            // let mut SLACK_WEBHOOK = String::new();

            let key = "SLACK_WEBHOOK";
            match std::env::var(key) {
                Ok(val) => {
                    exists_slack_webhook = true;
                    SLACK_WEBHOOK = val;
                },
                Err(e) => {
                    warn!(target: "server_log", "couldn't interpret {key}: {e}. slack messaging service unvailable.")
                },
            }

            let mut exists_discord_webhook = false;
            // let mut DISCORD_WEBHOOK = String::new();
            let key = "DISCORD_WEBHOOK";
            match std::env::var(key) {
                Ok(val) => {
                    exists_discord_webhook = true;
                    DISCORD_WEBHOOK = val;
                },
                Err(e) => {
                    warn!(target: "server_log", "couldn't interpret {key}: {e}. discord messaging service unvailable.")
                },
            }

            let mut messaging_flags = MessagingFlags::empty();
            if exists_slack_webhook {
                messaging_flags |= MessagingFlags::SLACK;
            }
            if exists_discord_webhook {
                messaging_flags |= MessagingFlags::DISCORD;
            }

            MESSAGING_FLAGS = messaging_flags;
        });
        MESSAGING_FLAGS
    }
}

#[derive(Clone)]
enum ClientVersion {
    V0,
    V1,
}

#[derive(Clone)]
struct ClientConnection {
    pubkey: Pubkey,
    miner_id: i64,
    client_version: ClientVersion,
    socket: Arc<Mutex<SplitSink<WebSocket, Message>>>,
}

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

// #[tokio::main]
#[tokio::main(flavor = "multi_thread", worker_threads = 12)]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    color_eyre::install().unwrap();
    dotenvy::dotenv().ok();
    let args = Args::parse();

    // MI: pure env filter
    // let env_filter_layer = tracing_subscriber::EnvFilter::try_from_default_env()
    //     .or_else(|_| tracing_subscriber::EnvFilter::try_new("info"))
    //     .unwrap();
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .or_else(|_| tracing_subscriber::EnvFilter::try_new("info"))
        .unwrap();

    // MI: complete layer definition
    let stdout_log_layer = tracing_subscriber::fmt::layer()
        .compact()
        // .pretty()
        .with_filter(env_filter)
        .with_filter(tracing_subscriber::filter::filter_fn(|metadata| {
            metadata.target() == "server_log"
        }));

    // MI: complete layer definition
    let server_logs = tracing_appender::rolling::daily("./logs", "ore-ppl-srv.log");
    let (server_logs, _guard) = tracing_appender::non_blocking(server_logs);
    let server_log_layer = tracing_subscriber::fmt::layer().with_writer(server_logs).with_filter(
        tracing_subscriber::filter::filter_fn(|metadata| metadata.target() == "server_log"),
    );

    // MI: complete layer definition
    let contribution_logs = tracing_appender::rolling::daily("./logs", "ore-ppl-contributions.log");
    let (contribution_logs, _guard) = tracing_appender::non_blocking(contribution_logs);
    let contribution_log_layer = tracing_subscriber::fmt::layer()
        .with_writer(contribution_logs)
        .with_filter(tracing_subscriber::filter::filter_fn(|metadata| {
            metadata.target() == "contribution_log"
        }));

    tracing_subscriber::registry()
        // .with(fmt::layer()) // MI: default layer() which is only required when followed by pure EnvFilter w/o layer definition
        .with(stdout_log_layer)
        .with(server_log_layer)
        .with(contribution_log_layer)
        .init();

    // let file_appender = tracing_appender::rolling::daily("./logs", "ore-ppl-srv.log");
    // let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);
    // tracing_subscriber::fmt().with_env_filter(filter_layer).with_writer(non_blocking).init();

    // load envs
    let wallet_path_str = std::env::var("WALLET_PATH").expect("WALLET_PATH must be set.");
    let key = "FEE_WALLET_PATH";
    let fee_wallet_path_str = match std::env::var(key) {
        Ok(val) => val,
        Err(_) => {
            info!(target: "server_log", "FEE_WALLET_PATH not set, using WALLET_PATH instead.");
            wallet_path_str.clone()
        },
    };
    let rpc_url = std::env::var("RPC_URL").expect("RPC_URL must be set.");
    let rpc_ws_url = std::env::var("RPC_WS_URL").expect("RPC_WS_URL must be set.");

    let powered_by_dbms = POWERED_BY_DBMS.get_or_init(|| {
        let key = "POWERED_BY_DBMS";
        match std::env::var(key) {
            Ok(val) => {
                PoweredByDbms::from_str(&val).expect("POWERED_BY_DBMS must be set correctly.")
            },
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
        info!(target: "server_log", "Powered by {} detected.", powered_by_dbms);
        // First, let's check if db file exist or not
        if !utils::exists_file(&dbms_settings.database_uri) {
            warn!(target: "server_log",
                "No existing database! New database will be created in the path: {}",
                dbms_settings.database_uri
            );
        } else {
            info!(target: "server_log",
                "The ore private pool db is already in place: {}. Opening...",
                dbms_settings.database_uri
            );
            dbms_settings.initialized = true;
        }
        // Second, we try to open a database connection.
        let conn = match Connection::open(dbms_settings.database_uri) {
            Ok(conn) => conn,
            Err(e) => {
                error!(target: "server_log", "Error connecting to database: {}.", e);
                return Err("Failed to connect to database.".into());
            },
        };

        // initialization check
        if !dbms_settings.initialized {
            info!(target: "server_log", "Initializing database...");
            // execute db init sql scripts
            // let command = fs::read_to_string("migrations/sqlite/init.sql").unwrap();
            let command = include_str!("../migrations/sqlite/init.sql");
            // conn.execute_batch(&command).unwrap();
            if let Err(e) = conn.execute_batch(&command) {
                error!(target: "server_log", "Error occurred during db initialization: {}", e);
                return Err("Failed during db initialization.".into());
            }
            dbms_settings.initialized = true;
            info!(target: "server_log", "Initialization completed.");
        }

        // retrive initialization completed flag
        let mut stmt =
            conn.prepare("SELECT id FROM init_completion WHERE init_completed = true")?;
        dbms_settings.corrupted = !stmt.exists([]).unwrap();

        // db file corruption check
        if dbms_settings.corrupted {
            error!(target: "server_log", "ore private pool db file corrupted.");
            return Err("ore private pool db file corrupted.".into());
        }
    }

    let database = Arc::new(Database::new(dbms_settings.database_uri.to_string()));

    let messaging_flags = get_messaging_flags();

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
        tracing::error!(target: "server_log", "❌ Failed to load wallet at: {}", wallet_path_str);
        return Err("Failed to find wallet path.".into());
    }

    let wallet = read_keypair_file(wallet_path)
        .expect("Failed to load keypair from file: {wallet_path_str}");
    let wallet_pubkey = wallet.pubkey();
    info!(target: "server_log", "loaded wallet {}", wallet_pubkey.to_string());

    // load fee wallet
    let wallet_path = Path::new(&fee_wallet_path_str);

    if !wallet_path.exists() {
        tracing::error!(target: "server_log", "❌ Failed to load fee wallet at: {}", fee_wallet_path_str);
        return Err("Failed to find fee wallet path.".into());
    }

    let fee_wallet = read_keypair_file(wallet_path)
        .expect("Failed to load keypair from file: {wallet_path_str}");
    info!(target: "server_log", "loaded fee wallet {}", wallet.pubkey().to_string());

    WALLET_PUBKEY.get_or_init(|| wallet_pubkey);

    info!(target: "server_log", "establishing rpc connection...");
    let rpc_client = RpcClient::new_with_commitment(rpc_url, CommitmentConfig::confirmed());

    info!(target: "server_log", "loading sol balance...");
    let balance = if let Ok(balance) = rpc_client.get_balance(&wallet_pubkey).await {
        balance
    } else {
        return Err("Failed to load balance".into());
    };

    info!(target: "server_log", "Balance: {:.9}", balance as f64 / LAMPORTS_PER_SOL as f64);

    if balance < 1_000_000 {
        return Err("Sol balance is too low!".into());
    }

    // MI
    let proof_pubkey = proof_pubkey(wallet_pubkey);
    debug!(target: "server_log", "PROOF ADDRESS: {:?}", proof_pubkey);
    let proof = if let Ok(loaded_proof) = get_proof(&rpc_client, wallet_pubkey).await {
        debug!(target: "server_log", "LOADED PROOF: \n{:?}", loaded_proof);
        loaded_proof
    } else {
        error!(target: "server_log", "Failed to load proof.");
        info!(target: "server_log", "Creating proof account...");

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
                info!(target: "server_log", "Sig: {}", sig.to_string());
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
        info!(target: "server_log", "Check if the mining pool record exists in the database");
        let mining_pool = database.get_pool_by_authority_pubkey(wallet_pubkey.to_string()).await;

        match mining_pool {
            Ok(_) => {},
            Err(DatabaseError::FailedToGetConnectionFromPool) => {
                panic!("Failed to get a connection from database pool");
            },
            Err(_) => {
                info!(target: "server_log", "Mining pool record missing from database. Inserting...");
                let proof_pubkey = utils::proof_pubkey(wallet_pubkey);
                let result = database
                    .add_new_pool(wallet_pubkey.to_string(), proof_pubkey.to_string())
                    .await;

                if result.is_err() {
                    panic!("Failed to add mining pool record in database");
                } else {
                    info!(target: "server_log", "Mining pool record added to database");
                }
            },
        }
        // info!(target: "server_log", "Mining pool record added to database");
        let mining_pool =
            database.get_pool_by_authority_pubkey(wallet_pubkey.to_string()).await.unwrap();

        mine_config = Arc::new(MineConfig { pool_id: mining_pool.id });

        info!(target: "server_log", "Check if current challenge for the pool exists in the database");
        let challenge = database.get_challenge_by_challenge(proof.challenge.to_vec()).await;

        match challenge {
            Ok(_) => {},
            Err(DatabaseError::FailedToGetConnectionFromPool) => {
                panic!("Failed to get a connection from database pool");
            },
            Err(_) => {
                info!(target: "server_log", "Challenge record missing from database. Inserting...");
                let new_challenge = models::InsertChallenge {
                    pool_id: mining_pool.id,
                    challenge: proof.challenge.to_vec(),
                    rewards_earned: None,
                };
                let result = database.add_new_challenge(new_challenge).await;

                if result.is_err() {
                    panic!("Failed to add challenge record in database");
                } else {
                    info!(target: "server_log", "Challenge record added to database");
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
        pong_tracking_processor(app_pongs, app_state).await;
    });

    let app_wallet = wallet_extension.clone();
    let app_proof = proof_ext.clone();
    // Establish webocket connection for tracking pool proof changes.
    tokio::spawn(async move {
        proof_tracking_processor(rpc_ws_url, app_wallet.miner_wallet.clone(), app_proof).await;
    });

    let (client_message_sender, client_message_receiver) =
        tokio::sync::mpsc::unbounded_channel::<ClientMessage>();

    // Handle client messages
    let app_ready_clients = ready_clients.clone();
    let app_proof = proof_ext.clone();
    let app_epoch_hashes = epoch_hashes.clone();
    let app_client_nonce_ranges = client_nonce_ranges.clone();
    let app_state = shared_state.clone();
    let app_pongs = pongs.clone();
    let app_min_difficulty = min_difficulty.clone();
    tokio::spawn(async move {
        client_message_processor(
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

    // Handle ready clients
    let rpc_client = Arc::new(rpc_client);
    let app_rpc_client = rpc_client.clone();
    let app_shared_state = shared_state.clone();
    let app_proof = proof_ext.clone();
    let app_epoch_hashes = epoch_hashes.clone();
    let app_ready_clients = ready_clients.clone();
    let app_nonce = nonce_ext.clone();
    let app_client_nonce_ranges = client_nonce_ranges.clone();
    let app_buffer_time = buffer_time.clone();
    let app_risk_time = risk_time.clone();
    tokio::spawn(async move {
        ready_clients_processor(
            app_rpc_client,
            app_shared_state,
            app_proof,
            app_epoch_hashes,
            app_ready_clients,
            app_nonce,
            app_client_nonce_ranges,
            app_buffer_time,
            app_risk_time,
        )
        .await;
    });

    let (slack_message_sender, slack_message_receiver) =
        tokio::sync::mpsc::unbounded_channel::<RewardsMessage>();

    // if exists slack webhook, handle slack messages to send
    if messaging_flags.contains(MessagingFlags::SLACK) {
        tokio::spawn(async move {
            unsafe {
                notification::slack_messaging_processor(
                    SLACK_WEBHOOK.clone(),
                    slack_message_receiver,
                )
                .await;
            }
        });
    }

    let (discord_message_sender, discord_message_receiver) =
        tokio::sync::mpsc::unbounded_channel::<RewardsMessage>();

    // if exists discord webhook, handle discord messages to send
    if messaging_flags.contains(MessagingFlags::DISCORD) {
        tokio::spawn(async move {
            unsafe {
                notification::discord_messaging_processor(
                    DISCORD_WEBHOOK.clone(),
                    discord_message_receiver,
                )
                .await;
            }
        });
    }

    // Start report routine
    let app_mine_config = mine_config.clone();
    let app_database = database.clone();
    tokio::spawn(async move {
        reporting_processor(reports_interval_in_hrs, app_mine_config, app_database).await;
    });

    let (mine_success_sender, mine_success_receiver) =
        tokio::sync::mpsc::unbounded_channel::<MessageInternalMineSuccess>();

    let (all_clients_sender, all_clients_receiver) =
        tokio::sync::mpsc::unbounded_channel::<MessageInternalAllClients>();

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
        pool_submission_processor(
            app_rpc_client,
            app_mine_config,
            app_shared_state,
            app_proof,
            app_epoch_hashes,
            app_wallet,
            app_nonce,
            app_dynamic_fee,
            app_dynamic_fee_url,
            app_priority_fee,
            app_priority_fee_cap,
            app_extra_fee_difficulty,
            app_extra_fee_percent,
            app_send_tpu_mine_tx,
            app_no_sound_notification,
            app_database,
            app_all_clients_sender,
            mine_success_sender,
            app_slack_message_sender,
            app_discord_message_sender,
            app_slack_difficulty,
            app_messaging_diff,
            app_buffer_time,
            app_risk_time,
        )
        .await;
    });

    let app_rpc_client = rpc_client.clone();
    let app_mine_config = mine_config.clone();
    let app_shared_state = shared_state.clone();
    let app_database = database.clone();
    let app_wallet = wallet_extension.clone();
    tokio::spawn(async move {
        pool_mine_success_processor(
            app_rpc_client,
            app_mine_config,
            app_shared_state,
            app_database,
            app_wallet,
            mine_success_receiver,
        )
        .await;
    });

    let app_shared_state = shared_state.clone();
    tokio::spawn(async move {
        messaging_all_clients_processor(app_shared_state, all_clients_receiver).await;
    });

    let cors = CorsLayer::new().allow_methods([Method::GET]).allow_origin(tower_http::cors::Any);

    let client_channel = client_message_sender.clone();
    let app_shared_state = shared_state.clone();
    let app = Router::new()
        .route("/", get(ws_handler))
        .route("/v1/ws", get(ws_handler_v1))
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

    tracing::info!(target: "server_log", "listening on {}", listener.local_addr().unwrap());

    let app_shared_state = shared_state.clone();
    tokio::spawn(async move {
        ping_check_processor(&app_shared_state).await;
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

    let _now = SystemTime::now().duration_since(UNIX_EPOCH).expect("Time went backwards").as_secs();

    // // Signed authentication message is only valid for 60 seconds
    // if (now - query_params.timestamp) >= 60 {
    //     return Err((StatusCode::UNAUTHORIZED, "Timestamp too old."));
    // }

    let powered_by_dbms = POWERED_BY_DBMS.get_or_init(|| {
        let key = "POWERED_BY_DBMS";
        match std::env::var(key) {
            Ok(val) => {
                PoweredByDbms::from_str(&val).expect("POWERED_BY_DBMS must be set correctly.")
            },
            Err(_) => PoweredByDbms::Unavailable,
        }
    });

    let pool_operator_wallet_pubkey = WALLET_PUBKEY.get().unwrap();

    // verify client
    if let Ok(user_pubkey) = Pubkey::from_str(pubkey) {
        if powered_by_dbms == &PoweredByDbms::Sqlite {
            info!(target: "server_log", "Check if the miner record exists in the database");
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
                    info!(target: "server_log", "Miner pubkey record missing from database. Inserting...");
                    let add_miner_result = database
                        .add_new_miner(user_pubkey.to_string(), true, "Enrolled".to_string())
                        .await;
                    miner =
                        database.get_miner_by_pubkey_str(user_pubkey.to_string()).await.unwrap();

                    // MI: vanilla, user_pubkey needs to signup with miner delegation pool
                    // let wallet_pubkey = user_pubkey;
                    let wallet_pubkey = *pool_operator_wallet_pubkey; // MI, all clients share operator/miner private pool

                    let db_pool =
                        database.get_pool_by_authority_pubkey(wallet_pubkey.to_string()).await;

                    let pool;
                    let mut add_pool_result = Ok::<(), DatabaseError>(());
                    match db_pool {
                        Ok(db_pool) => {
                            pool = db_pool;
                        },
                        Err(DatabaseError::QueryFailed) | Err(DatabaseError::InteractionFailed) => {
                            info!(target: "server_log", "Pool record missing from database. Inserting...");
                            add_pool_result = database
                                .add_new_pool(
                                    wallet_pubkey.to_string(),
                                    utils::proof_pubkey(wallet_pubkey).to_string(),
                                )
                                .await;
                            pool = database
                                .get_pool_by_authority_pubkey(wallet_pubkey.to_string())
                                .await
                                .unwrap();
                        },
                        Err(DatabaseError::FailedToGetConnectionFromPool) => {
                            error!(target: "server_log", "Failed to get database pool connection.");
                            return Err((
                                StatusCode::INTERNAL_SERVER_ERROR,
                                "Internal Server Error",
                            ));
                        },
                        Err(_) => {
                            error!(target: "server_log", "DB Error: Catch all.");
                            return Err((
                                StatusCode::INTERNAL_SERVER_ERROR,
                                "Internal Server Error",
                            ));
                        },
                    }

                    if add_miner_result.is_ok() && add_pool_result.is_ok() {
                        let new_reward = InsertReward { miner_id: miner.id, pool_id: pool.id };
                        let result = database.add_new_reward(new_reward).await;

                        if result.is_ok() {
                            info!(target: "server_log", "Miner and rewards tracker added to database");
                        } else {
                            error!(target: "server_log", "Failed to add miner rewards tracker to database");
                            return Err((
                                StatusCode::UNAUTHORIZED,
                                "Failed to add miner rewards tracker to database",
                            ));
                        }
                        info!(target: "server_log", "Miner record added to database");
                    } else {
                        error!(target: "server_log", "Failed to add miner record to database");
                        return Err((
                            StatusCode::UNAUTHORIZED,
                            "Failed to add miner record to database",
                        ));
                    }
                },
                Err(DatabaseError::FailedToGetConnectionFromPool) => {
                    error!(target: "server_log", "Failed to get database pool connection.");
                    return Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error"));
                },
                Err(_) => {
                    error!(target: "server_log", "DB Error: Catch all.");
                    return Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error"));
                },
            }

            if !miner.enabled {
                return Err((StatusCode::UNAUTHORIZED, "pubkey is not authorized to mine"));
            }

            if let Ok(signature) = Signature::from_str(signed_msg) {
                let ts_msg = msg_timestamp.to_le_bytes();

                if signature.verify(&user_pubkey.to_bytes(), &ts_msg) {
                    info!(target: "server_log", "Client: {addr} connected with pubkey {pubkey}.");
                    return Ok(ws.on_upgrade(move |socket| {
                        handle_socket(
                            socket,
                            addr,
                            user_pubkey,
                            miner.id,
                            ClientVersion::V0,
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
                    info!(target: "server_log", "Client: {addr} connected with pubkey {pubkey}.");
                    return Ok(ws.on_upgrade(move |socket| {
                        handle_socket(
                            socket,
                            addr,
                            user_pubkey,
                            // MI: default miner_id for non-dbms
                            i64::MAX,
                            ClientVersion::V0,
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

#[debug_handler]
async fn ws_handler_v1(
    ws: WebSocketUpgrade,
    TypedHeader(auth_header): TypedHeader<axum_extra::headers::Authorization<Basic>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(app_state): State<Arc<RwLock<AppState>>>,
    Extension(client_channel): Extension<UnboundedSender<ClientMessage>>,
    Extension(database): Extension<Arc<Database>>,
    query_params: Query<WsQueryParams>,
) -> impl IntoResponse {
    let msg_timestamp = query_params.timestamp;

    let pubkey = auth_header.username();
    let signed_msg = auth_header.password();

    let _now = SystemTime::now().duration_since(UNIX_EPOCH).expect("Time went backwards").as_secs();

    let powered_by_dbms = POWERED_BY_DBMS.get_or_init(|| {
        let key = "POWERED_BY_DBMS";
        match std::env::var(key) {
            Ok(val) => {
                PoweredByDbms::from_str(&val).expect("POWERED_BY_DBMS must be set correctly.")
            },
            Err(_) => PoweredByDbms::Unavailable,
        }
    });

    let pool_operator_wallet_pubkey = WALLET_PUBKEY.get().unwrap();

    // verify client
    if let Ok(user_pubkey) = Pubkey::from_str(pubkey) {
        if powered_by_dbms == &PoweredByDbms::Sqlite {
            info!(target: "server_log", "Check if the miner record exists in the database");
            let db_miner = database.get_miner_by_pubkey_str(pubkey.to_string()).await;

            let miner;
            match db_miner {
                Ok(db_miner) => {
                    miner = db_miner;
                },
                Err(DatabaseError::QueryFailed) | Err(DatabaseError::InteractionFailed) => {
                    info!(target: "server_log", "Miner pubkey record missing from database. Inserting...");
                    let add_miner_result = database
                        .add_new_miner(user_pubkey.to_string(), true, "Enrolled".to_string())
                        .await;
                    miner =
                        database.get_miner_by_pubkey_str(user_pubkey.to_string()).await.unwrap();

                    let wallet_pubkey = *pool_operator_wallet_pubkey; // MI, all clients share operator/miner private pool

                    let db_pool =
                        database.get_pool_by_authority_pubkey(wallet_pubkey.to_string()).await;

                    let pool;
                    let mut add_pool_result = Ok::<(), DatabaseError>(());
                    match db_pool {
                        Ok(db_pool) => {
                            pool = db_pool;
                        },
                        Err(DatabaseError::QueryFailed) | Err(DatabaseError::InteractionFailed) => {
                            info!(target: "server_log", "Pool record missing from database. Inserting...");
                            add_pool_result = database
                                .add_new_pool(
                                    wallet_pubkey.to_string(),
                                    utils::proof_pubkey(wallet_pubkey).to_string(),
                                )
                                .await;
                            pool = database
                                .get_pool_by_authority_pubkey(wallet_pubkey.to_string())
                                .await
                                .unwrap();
                        },
                        Err(DatabaseError::FailedToGetConnectionFromPool) => {
                            error!(target: "server_log", "Failed to get database pool connection.");
                            return Err((
                                StatusCode::INTERNAL_SERVER_ERROR,
                                "Internal Server Error",
                            ));
                        },
                        Err(_) => {
                            error!(target: "server_log", "DB Error: Catch all.");
                            return Err((
                                StatusCode::INTERNAL_SERVER_ERROR,
                                "Internal Server Error",
                            ));
                        },
                    }

                    if add_miner_result.is_ok() && add_pool_result.is_ok() {
                        let new_reward = InsertReward { miner_id: miner.id, pool_id: pool.id };
                        let result = database.add_new_reward(new_reward).await;

                        if result.is_ok() {
                            info!(target: "server_log", "Miner and rewards tracker added to database");
                        } else {
                            error!(target: "server_log", "Failed to add miner rewards tracker to database");
                            return Err((
                                StatusCode::UNAUTHORIZED,
                                "Failed to add miner rewards tracker to database",
                            ));
                        }
                        info!(target: "server_log", "Miner record added to database");
                    } else {
                        error!(target: "server_log", "Failed to add miner record to database");
                        return Err((
                            StatusCode::UNAUTHORIZED,
                            "Failed to add miner record to database",
                        ));
                    }
                },
                Err(DatabaseError::FailedToGetConnectionFromPool) => {
                    error!(target: "server_log", "Failed to get database pool connection.");
                    return Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error"));
                },
                Err(_) => {
                    error!(target: "server_log", "DB Error: Catch all.");
                    return Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error"));
                },
            }

            if !miner.enabled {
                return Err((StatusCode::UNAUTHORIZED, "pubkey is not authorized to mine"));
            }

            if let Ok(signature) = Signature::from_str(signed_msg) {
                let ts_msg = msg_timestamp.to_le_bytes();

                if signature.verify(&user_pubkey.to_bytes(), &ts_msg) {
                    info!(target: "server_log", "Client: {addr} connected with pubkey {pubkey}.");
                    return Ok(ws.on_upgrade(move |socket| {
                        handle_socket(
                            socket,
                            addr,
                            user_pubkey,
                            miner.id,
                            ClientVersion::V1,
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
                    info!(target: "server_log", "Client: {addr} connected with pubkey {pubkey}.");
                    return Ok(ws.on_upgrade(move |socket| {
                        handle_socket(
                            socket,
                            addr,
                            user_pubkey,
                            // MI: default miner_id for non-dbms
                            i64::MAX,
                            ClientVersion::V1,
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
    client_version: ClientVersion,
    rw_app_state: Arc<RwLock<AppState>>,
    client_channel: UnboundedSender<ClientMessage>,
) {
    if socket.send(axum::extract::ws::Message::Ping(vec![1, 2, 3])).await.is_ok() {
        debug!(target: "server_log", "Pinged {who}... pubkey: {who_pubkey}");
    } else {
        error!(target: "server_log", "could not ping {who} pubkey: {who_pubkey}");

        // if we can't ping we can't do anything, return to close the connection
        return;
    }

    let (sender, mut receiver) = socket.split();
    let mut app_state = rw_app_state.write().await;
    if app_state.sockets.contains_key(&who) {
        info!(target: "server_log", "Socket addr: {who} already has an active connection");
        return;
    } else {
        let new_client_connection = ClientConnection {
            pubkey: who_pubkey,
            miner_id: who_miner_id,
            client_version,
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

    info!(target: "server_log", "Client: {} disconnected!", who_pubkey.to_string());
}

fn process_message(
    msg: Message,
    who: SocketAddr,
    client_channel: UnboundedSender<ClientMessage>,
) -> ControlFlow<(), ()> {
    match msg {
        Message::Text(_t) => {
            // info!(target: "server_log", ">>> {who} sent str: {t:?}");
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
                                error!(target: "server_log", "Client contribution sig verification failed.");
                            }
                        } else {
                            error!(target: "server_log", "Failed to parse into Signature.");
                        }
                    } else {
                        error!(target: "server_log", "Failed to parse signed message from client.");
                    }
                },
                _ => {
                    error!(target: "server_log", ">>> {} sent an invalid message", who);
                },
            }
        },
        Message::Close(c) => {
            if let Some(cf) = c {
                info!(target: "server_log", ">>> {} sent close with code {} and reason `{}`", who, cf.code, cf.reason);
            } else {
                info!(target: "server_log", ">>> {who} somehow sent close message without CloseFrame");
            }
            return ControlFlow::Break(());
        },
        Message::Pong(_v) => {
            let msg = ClientMessage::Pong(who);
            let _ = client_channel.send(msg);
        },
        Message::Ping(_v) => {
            //info!(target: "server_log", ">>> {who} sent ping with {v:?}");
        },
    }

    ControlFlow::Continue(())
}

fn styles() -> Styles {
    Styles::styled()
        .header(AnsiColor::Red.on_default() | Effects::BOLD)
        .usage(AnsiColor::Red.on_default() | Effects::BOLD)
        .literal(AnsiColor::Blue.on_default() | Effects::BOLD)
        .placeholder(AnsiColor::Green.on_default())
}
