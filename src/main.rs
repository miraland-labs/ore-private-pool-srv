use {
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
    drillx::Solution,
    dynamic_fee as pfee,
    futures::{stream::SplitSink, SinkExt, StreamExt},
    notification::RewardsMessage,
    ore_api::{consts::BUS_COUNT, error::OreError, state::Proof},
    rand::Rng,
    serde::Deserialize,
    solana_account_decoder::UiAccountEncoding,
    solana_client::{
        client_error::ClientErrorKind,
        nonblocking::{pubsub_client::PubsubClient, rpc_client::RpcClient},
        rpc_config::{RpcAccountInfoConfig, RpcSendTransactionConfig},
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
        net::SocketAddr,
        ops::{ControlFlow, Div, Range},
        path::Path,
        str::FromStr,
        sync::{
            atomic::{AtomicBool, Ordering::Relaxed},
            Arc,
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
    tracing::{error, info, warn},
    utils::{
        get_auth_ix, get_clock, get_config_and_proof, get_cutoff, get_cutoff_with_risk,
        get_mine_ix, get_proof, get_register_ix, get_reset_ix, proof_pubkey, ORE_TOKEN_DECIMALS,
    },
};

mod dynamic_fee;
mod notification;
mod utils;

// MI
// min hash power is matching with ore BASE_REWARD_RATE_MIN_THRESHOLD
// min difficulty, matching with MIN_HASHPOWER.
const MIN_HASHPOWER: u64 = 5;
const MIN_DIFF: u32 = 5;

// MI: if 0, rpc node will retry the tx until it is finalized or until the blockhash expires
const RPC_RETRIES: usize = 3; // 5

const SUBMIT_LIMIT: u32 = 5;
const CHECK_LIMIT: usize = 50; // 30

#[derive(Clone)]
struct ClientConnection {
    pubkey: Pubkey,
    socket: Arc<Mutex<SplitSink<WebSocket, Message>>>,
}

struct AppState {
    sockets: HashMap<SocketAddr, ClientConnection>,
}

pub struct MessageInternalAllClients {
    text: String,
}

pub struct MessageInternalMineSuccess {
    difficulty: u32,
    total_balance: f64,
    rewards: u64,
    total_hashpower: u64,
    submissions: HashMap<Pubkey, (u32, u64)>,
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
    best_hash: BestHash,
    submissions: HashMap<Pubkey, (/* diff: */ u32, /* hashpower: */ u64)>,
}

pub struct BestHash {
    solution: Option<Solution>,
    difficulty: u32,
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
    let rpc_url = std::env::var("RPC_URL").expect("RPC_URL must be set.");
    let rpc_ws_url = std::env::var("RPC_WS_URL").expect("RPC_WS_URL must be set.");

    // let slack_webhook = std::env::var("SLACK_WEBHOOK").expect("SLACK_WEBHOOK must be set.");
    // let discord_webhook = std::env::var("DISCORD_WEBHOOK").expect("DISCORD_WEBHOOK must be set.");

    //TODO: further verify those webhook existences are valid
    let mut exists_slack_webhook = false;
    let mut slack_webhook = String::new();

    let key = "SLACK_WEBHOOK";
    match std::env::var(key) {
        Ok(val) => {
            exists_slack_webhook = true;
            slack_webhook = val;
        },
        // Err(e) => println!("couldn't interpret {key}: {e}"),
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
        // Err(e) => println!("couldn't interpret {key}: {e}"),
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

    info!("establishing rpc connection...");
    let rpc_client = RpcClient::new_with_commitment(rpc_url, CommitmentConfig::confirmed());

    info!("loading sol balance...");
    let balance = if let Ok(balance) = rpc_client.get_balance(&wallet_pubkey).await {
        balance
    } else {
        return Err("Failed to load balance".into());
    };

    info!("Balance: {:.2}", balance as f64 / LAMPORTS_PER_SOL as f64);

    if balance < 1_000_000 {
        return Err("Sol balance is too low!".into());
    }

    // MI
    let proof_pubkey = proof_pubkey(wallet_pubkey);
    info!("PROOF ADDRESS: {:?}", proof_pubkey);
    let proof = if let Ok(loaded_proof) = get_proof(&rpc_client, wallet_pubkey).await {
        info!("LOADED PROOF: \n{:?}", loaded_proof);
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

    let epoch_hashes = Arc::new(RwLock::new(EpochHashes {
        best_hash: BestHash { solution: None, difficulty: 0 },
        submissions: HashMap::new(),
    }));

    let wallet_extension = Arc::new(wallet);
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
        proof_tracking_system(rpc_ws_url, app_wallet, app_proof).await;
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

    // MI
    let (slack_message_sender, slack_message_receiver) =
        tokio::sync::mpsc::unbounded_channel::<RewardsMessage>();

    // if exists slack webhook, handle slack messages to send
    // if exists_slack_webhook {
    if messaging_flags.contains(MessagingFlags::SLACK) {
        tokio::spawn(async move {
            notification::slack_messaging_system(slack_webhook, slack_message_receiver).await;
        });
    }

    // MI
    let (discord_message_sender, discord_message_receiver) =
        tokio::sync::mpsc::unbounded_channel::<RewardsMessage>();

    // if exists discord webhook, handle discord messages to send
    // if exists_discord_webhook {
    if messaging_flags.contains(MessagingFlags::DISCORD) {
        tokio::spawn(async move {
            notification::discord_messaging_system(discord_webhook, discord_message_receiver).await;
        });
    }

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
                            let end = *nonce;
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
    let app_no_sound_notification = no_sound_notification.clone();
    let app_all_clients_sender = all_clients_sender.clone();
    let app_slack_message_sender = slack_message_sender.clone();
    let app_discord_message_sender = discord_message_sender.clone();
    let app_slack_difficulty = slack_difficulty.clone();
    let app_messaging_diff = messaging_diff.clone();
    let app_buffer_time = buffer_time.clone();
    let app_risk_time = risk_time.clone();
    tokio::spawn(async move {
        let rpc_client = app_rpc_client;
        let slack_message_sender = app_slack_message_sender;
        let discord_message_sender = app_discord_message_sender;
        let slack_difficulty = app_slack_difficulty;
        let messaging_diff = app_messaging_diff;
        // MI
        let mut solution_is_none_counter = 0;
        let mut num_waiting = 0;
        loop {
            let lock = app_proof.lock().await;
            let mut old_proof = lock.clone();
            drop(lock);

            let cutoff = if (*app_risk_time).gt(&0) {
                get_cutoff_with_risk(&rpc_client, proof, *app_buffer_time, *app_risk_time).await
            } else {
                get_cutoff(&rpc_client, proof, *app_buffer_time).await
            };
            if cutoff <= 0 {
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
                        let signer = app_wallet.clone();

                        let bus = rand::thread_rng().gen_range(0..BUS_COUNT);

                        let mut success = false;
                        let reader = app_epoch_hashes.read().await;
                        let best_solution = reader.best_hash.solution.clone();
                        let num_submissions = reader.submissions.len();
                        // let submissions = reader.submissions.clone();
                        // MI, wait until all active miners' submissions received, and waiting times
                        // < 6. if min_difficulty is relative high, may
                        // always false num_submissions depends on min diff
                        if num_submissions != num_active_miners && num_waiting < 6 {
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
                                    "Submitting attempt {} with ✨ diff {} ✨ of {} qualified submissions at {}.",
                                    i + 1,
                                    difficulty,
                                    num_submissions,
                                    Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
                                );
                                info!(
                                    "Submission Challenge: {}",
                                    BASE64_STANDARD.encode(old_proof.challenge)
                                );

                                info!("Getting current/latest config and proof.");
                                let config = if let Ok((loaded_config, loaded_proof)) =
                                    get_config_and_proof(&rpc_client, signer.pubkey()).await
                                {
                                    info!(
                                        "Current/latest pool Challenge: {}",
                                        BASE64_STANDARD.encode(loaded_proof.challenge)
                                    );

                                    if !best_solution.is_valid(&loaded_proof.challenge) {
                                        error!("❌ SOLUTION IS NOT VALID ANYMORE!");
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
                                let should_add_reset_ix = if let Some(config) = config {
                                    let time_to_reset =
                                        (config.last_reset_at + 300) - current_timestamp as i64;
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
                                    // MI, vanilla: if stick to static/fix price no matter diff, use this stmt.
                                    // app_priority_fee.unwrap_or(0)

                                    // MI: consider to pay more fee for precious diff even with static fee mode
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

                                let send_cfg = RpcSendTransactionConfig {
                                    skip_preflight: true,
                                    preflight_commitment: Some(CommitmentLevel::Confirmed),
                                    encoding: Some(UiTransactionEncoding::Base64),
                                    max_retries: Some(RPC_RETRIES),
                                    min_context_slot: None,
                                };

                                if let Ok((hash, _slot)) = rpc_client
                                    .get_latest_blockhash_with_commitment(rpc_client.commitment())
                                    .await
                                {
                                    let mut tx =
                                        Transaction::new_with_payer(&ixs, Some(&signer.pubkey()));

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

                                            if !*app_no_sound_notification {
                                                utils::play_sound();
                                            }

                                            // update proof
                                            // limit number of checking no more than CHECK_LIMIT
                                            let mut num_checking = 0;
                                            loop {
                                                info!("Waiting for proof challenge update");
                                                let latest_proof =
                                                    { app_proof.lock().await.clone() };

                                                if old_proof.challenge.eq(&latest_proof.challenge) {
                                                    info!("Proof challenge not updated yet..");
                                                    old_proof = latest_proof;
                                                    tokio::time::sleep(Duration::from_millis(
                                                        1_000,
                                                    ))
                                                    .await;
                                                    num_checking += 1;
                                                    if num_checking < CHECK_LIMIT {
                                                        continue;
                                                    } else {
                                                        warn!("No challenge update detected after {CHECK_LIMIT} checkpoints. No more waiting, just keep going...");
                                                        break;
                                                    }
                                                } else {
                                                    info!(
                                                    "Proof challenge updated! Checking rewards earned."
                                                );
                                                    let balance = (latest_proof.balance as f64)
                                                        / 10f64.powf(ORE_TOKEN_DECIMALS as f64);
                                                    info!("New balance: {}", balance);
                                                    let rewards =
                                                        latest_proof.balance - old_proof.balance;
                                                    let dec_rewards = (rewards as f64)
                                                        / 10f64.powf(ORE_TOKEN_DECIMALS as f64);
                                                    info!("Earned: {} ORE", dec_rewards);

                                                    let submissions = {
                                                        app_epoch_hashes
                                                            .read()
                                                            .await
                                                            .submissions
                                                            .clone()
                                                    };

                                                    let mut total_hashpower: u64 = 0;

                                                    for submission in submissions.iter() {
                                                        total_hashpower += submission.1 .1
                                                    }

                                                    let _ = mine_success_sender.send(
                                                        MessageInternalMineSuccess {
                                                            difficulty,
                                                            total_balance: balance,
                                                            rewards,
                                                            total_hashpower,
                                                            submissions,
                                                        },
                                                    );

                                                    {
                                                        let mut mut_proof = app_proof.lock().await;
                                                        *mut_proof = latest_proof;
                                                    }

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
                                                        mut_epoch_hashes.best_hash.solution = None;
                                                        mut_epoch_hashes.best_hash.difficulty = 0;
                                                        mut_epoch_hashes.submissions =
                                                            HashMap::new();
                                                    }

                                                    // unset mining pause flag to start new mining
                                                    // mission
                                                    info!("resume new mining mission");
                                                    PAUSED.store(false, Relaxed);

                                                    // last one, notify slack and other messaging channels if necessary
                                                    if difficulty.ge(&*slack_difficulty)
                                                        || difficulty.ge(&*messaging_diff)
                                                    {
                                                        // if exists_slack_webhook {
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

                                                        // if exists_discord_webhook {
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
                                            break;
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
                                mut_epoch_hashes.submissions = HashMap::new();
                            }

                            // unset mining pause flag to start new mining mission
                            info!("resume new mining mission");
                            PAUSED.store(false, Relaxed);
                        }
                        tokio::time::sleep(Duration::from_millis(500)).await;
                    } else {
                        solution_is_none_counter += 1;
                        if solution_is_none_counter % 5 == 0 {
                            info!("No best solution yet.");
                        }
                        tokio::time::sleep(Duration::from_millis(1_000)).await;
                    }
                }
            } else {
                // reset none solution counter
                solution_is_none_counter = 0;
                tokio::time::sleep(Duration::from_secs(cutoff as u64)).await;
            };
        }
    });

    let app_shared_state = shared_state.clone();
    let app_rpc_client = rpc_client.clone();
    let app_wallet = wallet_extension.clone();
    tokio::spawn(async move {
        loop {
            let mut sol_balance_checking = 0_u64;
            while let Some(msg) = mine_success_receiver.recv().await {
                {
                    let decimals = 10f64.powf(ORE_TOKEN_DECIMALS as f64);
                    let pool_rewards_dec = (msg.rewards as f64).div(decimals);
                    let shared_state = app_shared_state.read().await;
                    let len = shared_state.sockets.len();
                    for (_socket_addr, socket_sender) in shared_state.sockets.iter() {
                        let pubkey = socket_sender.pubkey;

                        if let Some((supplied_diff, pubkey_hashpower)) =
                            msg.submissions.get(&pubkey)
                        {
                            // let hashpower_percent =
                            //     (*pubkey_hashpower as f64).div(msg.total_hashpower as f64);

                            // let hashpower =
                            //     MIN_HASHPOWER * 2u64.pow(*supplied_diff as u32 - MIN_DIFF);

                            let hashpower_percent = (*pubkey_hashpower as u128)
                                .saturating_mul(1_000_000)
                                .saturating_div(msg.total_hashpower as u128);

                            // let decimals = 10f64.powf(ORE_TOKEN_DECIMALS as f64);
                            // let earned_rewards = hashpower_percent
                            //     .mul(msg.rewards)
                            //     .mul(decimals)
                            //     .floor()
                            //     .div(decimals);
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
                    if let Ok(balance) = app_rpc_client.get_balance(&app_wallet.pubkey()).await {
                        info!("Sol Balance: {:.2}", balance as f64 / LAMPORTS_PER_SOL as f64);
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
    Extension(wallet): Extension<Arc<Keypair>>,
) -> impl IntoResponse {
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "text/text")
        .body(wallet.pubkey().to_string())
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
    Extension(client_channel): Extension<UnboundedSender<ClientMessage>>,
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

    // verify client
    if let Ok(user_pubkey) = Pubkey::from_str(pubkey) {
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
                    handle_socket(socket, addr, user_pubkey, app_state, client_channel)
                }));
            } else {
                return Err((StatusCode::UNAUTHORIZED, "Sig verification failed"));
            }
        } else {
            return Err((StatusCode::UNAUTHORIZED, "Invalid signature"));
        }
    } else {
        return Err((StatusCode::UNAUTHORIZED, "Invalid pubkey"));
    }
}

async fn handle_socket(
    mut socket: WebSocket,
    who: SocketAddr,
    who_pubkey: Pubkey,
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
        let new_client_connection =
            ClientConnection { pubkey: who_pubkey, socket: Arc::new(Mutex::new(sender)) };
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
                                error!("Client submission sig verification failed.");
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
        info!("RPC ws connection established!");

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
                                // drop(app_proof);
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
                        if reader.sockets.get(&addr).is_none() {
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
                                    let mut epoch_hashes = epoch_hashes.write().await;
                                    epoch_hashes.submissions.insert(pubkey, (diff, hashpower));
                                    if diff > epoch_hashes.best_hash.difficulty {
                                        epoch_hashes.best_hash.difficulty = diff;
                                        epoch_hashes.best_hash.solution = Some(solution);
                                    }
                                    drop(epoch_hashes);
                                }
                                tokio::time::sleep(Duration::from_millis(100)).await;
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

fn styles() -> Styles {
    Styles::styled()
        .header(AnsiColor::Red.on_default() | Effects::BOLD)
        .usage(AnsiColor::Red.on_default() | Effects::BOLD)
        .literal(AnsiColor::Blue.on_default() | Effects::BOLD)
        .placeholder(AnsiColor::Green.on_default())
}
