use std::{
    collections::{HashMap, HashSet},
    net::SocketAddr,
    ops::{ControlFlow, Div, Mul, Range},
    path::Path,
    str::FromStr,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use self::models::*;
use dashmap::DashMap;
use dynamic_fee as pfee;
use ::ore_utils::AccountDeserialize;
use app_database::{AppDatabase, AppDatabaseError};

use axum::{
    extract::{
        ws::{Message, WebSocket},
        ConnectInfo, Query, State, WebSocketUpgrade,
    },
    http::{Response, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Extension, Router, debug_handler,
};
use axum_extra::{headers::authorization::Basic, TypedHeader};
use base64::{prelude::BASE64_STANDARD, Engine};
use clap::{
    builder::{
        styling::{AnsiColor, Effects},
        Styles,
    },
    command, Parser, Subcommand,
};
use deadpool_diesel::mysql::{Manager, Pool};
use diesel::{
    query_dsl::methods::FilterDsl,
    sql_types::{Bool, Text},
    MysqlConnection, QueryDsl, RunQueryDsl, SelectableHelper,
};
use drillx::Solution;
use futures::{stream::SplitSink, SinkExt, StreamExt};
use ore_api::{
    consts::{BUS_ADDRESSES, BUS_COUNT},
    state::Proof,
};
use ore_utils::{
    get_auth_ix, get_cutoff, get_mine_ix, get_ore_mint, get_proof, get_proof_and_best_bus,
    get_proof_and_config_with_busses, get_register_ix, get_reset_ix, proof_pubkey,
    ORE_TOKEN_DECIMALS,
};
use rand::Rng;
use serde::Deserialize;
use solana_account_decoder::UiAccountEncoding;
use solana_client::{
    nonblocking::{pubsub_client::PubsubClient, rpc_client::RpcClient},
    rpc_config::{RpcAccountInfoConfig, RpcProgramAccountsConfig},
};
use solana_sdk::{
    commitment_config::CommitmentConfig,
    compute_budget::ComputeBudgetInstruction,
    native_token::LAMPORTS_PER_SOL,
    pubkey::Pubkey,
    signature::{read_keypair_file, Keypair, Signature},
    signer::Signer,
    system_instruction,
    transaction::Transaction,
};
use spl_associated_token_account::get_associated_token_address;
use tokio::{
    io::AsyncReadExt,
    sync::{
        mpsc::{UnboundedReceiver, UnboundedSender},
        Mutex, RwLock,
    },
};
use tower_http::trace::{DefaultMakeSpan, TraceLayer};
use tracing::{error, info, warn};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

mod app_database;
mod dynamic_fee;
mod models;
mod ore_utils;
mod schema;
mod utils;

const MIN_DIFF: u32 = 8;
const MIN_HASHPOWER: u64 = 5;

struct AppState {
    sockets: HashMap<SocketAddr, (Pubkey, Arc<Mutex<SplitSink<WebSocket, Message>>>)>,
    // miner_sockets: HashMap<i32, (Pubkey, Arc<Mutex<SplitSink<WebSocket, Message>>>)>,
    miner_sockets: HashMap<Pubkey, Arc<Mutex<SplitSink<WebSocket, Message>>>>,
}

pub struct MessageInternalMineSuccess {
    difficulty: u32,
    total_balance: f64,
    rewards: u64,
    total_hashpower: u64,
    submissions: HashMap<Pubkey, (u32, u64)>,
}

// pub struct MessageInternalMineSuccess {
//     difficulty: u32,
//     total_balance: f64,
//     pool_id: i32,
//     rewards: u64,
//     total_hashpower: u64,
//     submissions: Vec<Submission>,
// }

#[derive(Debug)]
pub enum ClientMessage {
    Ready(SocketAddr),
    Mining(SocketAddr),
    BestSolution(SocketAddr, Solution, Pubkey),
}

pub struct EpochHashes {
    best_hash: BestHash,
    submissions: HashMap<Pubkey, (/* diff: */ u32, /* hashpower: */ u64)>, // MI
}

pub struct BestHash {
    solution: Option<Solution>,
    difficulty: u32,
}

pub struct Config {
    password: String,
    whitelist: Option<HashSet<Pubkey>>,
    // pool_id: i32,
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
        value_name = "whitelist",
        help = "Path to whitelist of allowed miners",
        default_value = None,
        global = true
    )]
    whitelist: Option<String>,

    #[arg(
        long,
        value_name = "signup cost",
        help = "Amount of sol users must send to sign up for the pool",
        default_value = "0",
        global = true
    )]
    signup_cost: u64,

    #[arg(long, help = "Powered by dbms.", global = true)]
    powered_by_dbms: bool,

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
        value_name = "FEE_MICROLAMPORTS",
        help = "Price to pay for compute units when dynamic fee flag is off, or dynamic fee is unavailable.",
        default_value = "20000",
        global = true
    )]
    priority_fee: Option<u64>,

    #[arg(
        long,
        value_name = "FEE_CAP_MICROLAMPORTS",
        help = "Max price to pay for compute units when dynamic fees are enabled.",
        default_value = "500000",
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
        help = "The expected min difficulty to submit for miner.",
        default_value = "18"
    )]
    pub expected_min_difficulty: u32,

    #[arg(
        long,
        short,
        value_name = "EXTRA_FEE_DIFFICULTY",
        help = "The min difficulty that the miner thinks deserves to pay more priority fee.",
        default_value = "27"
    )]
    pub extra_fee_difficulty: u32,

    #[arg(
        long,
        short,
        value_name = "EXTRA_FEE_PERCENT",
        help = "The extra percentage that the miner feels deserves to pay more of the priority fee. A positive integer in the range 0..100 [inclusive] is preferred (although integer > 100 is possible, but not recommended), and the final priority fee cannot exceed the priority fee cap.",
        default_value = "0"
    )]
    pub extra_fee_percent: u64,

    /// Mine with sound notification on/off
    #[arg(
        long,
        value_name = "NO_SOUND_NOTIFICATION",
        help = "Sound notification off by default",
        default_value = "false",
        global = true
    )]
    pub no_sound_notification: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    color_eyre::install().unwrap();
    dotenv::dotenv().ok();
    let args = Args::parse();
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "ore_hq_server=debug,tower_http=debug".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    // load envs
    let wallet_path_str = std::env::var("WALLET_PATH").expect("WALLET_PATH must be set.");
    let rpc_url = std::env::var("RPC_URL").expect("RPC_URL must be set.");
    let rpc_ws_url = std::env::var("RPC_WS_URL").expect("RPC_WS_URL must be set.");
    let password = std::env::var("PASSWORD").expect("PASSWORD must be set.");
    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set.");
    // let mut powered_by_dbms = std::env::var("POWERED_BY_DBMS").expect("POWERED_BY_DBMS must be set.");

    // MI
    let app_database = Arc::new(AppDatabase::new(database_url));
    let virtual_database: Arc<DashMap<Pubkey, InsertSubmission>> = DashMap::new().into();

    let whitelist = if let Some(whitelist) = args.whitelist {
        let file = Path::new(&whitelist);
        if file.exists() {
            // load file
            let mut pubkeys = HashSet::new();
            if let Ok(mut file) = tokio::fs::File::open(file).await {
                let mut file_contents = String::new();
                file.read_to_string(&mut file_contents)
                    .await
                    .ok()
                    .expect("Failed to read whitelist file");
                drop(file);

                for (i, line) in file_contents.lines().enumerate() {
                    if let Ok(pubkey) = Pubkey::from_str(line) {
                        pubkeys.insert(pubkey);
                    } else {
                        let err = format!(
                            "Failed to create pubkey from line {} with value: {}",
                            i, line
                        );
                        error!(err);
                    }
                }
            } else {
                return Err("Failed to open whitelist file".into());
            }
            Some(pubkeys)
        } else {
            return Err("Whitelist at specified file path doesn't exist".into());
        }
    } else {
        None
    };

    // let priority_fee = Arc::new(Mutex::new(args.priority_fee));
    let priority_fee = Arc::new(args.priority_fee);
    let priority_fee_cap = Arc::new(args.priority_fee_cap);

    let buffer_time = Arc::new(args.buffer_time);

    // let expected_min_difficulty = Arc::new(args.expected_min_difficulty);
    let min_difficulty = Arc::new(args.expected_min_difficulty);
    let extra_fee_difficulty = Arc::new(args.extra_fee_difficulty);
    let extra_fee_percent = Arc::new(args.extra_fee_percent);

    let powered_by_dbms = args.powered_by_dbms;
    let dynamic_fee = Arc::new(args.dynamic_fee);
    let dynamic_fee_url = Arc::new(args.dynamic_fee_url);

    let no_sound_notification = Arc::new(args.no_sound_notification);

    // load wallet
    let wallet_path = Path::new(&wallet_path_str);

    if !wallet_path.exists() {
        tracing::error!("Failed to load wallet at: {}", wallet_path_str);
        return Err("Failed to find wallet path.".into());
    }

    let wallet = read_keypair_file(wallet_path)
        .expect("Failed to load keypair from file: {wallet_path_str}");
    println!("loaded wallet {}", wallet.pubkey().to_string());

    println!("establishing rpc connection...");
    let rpc_client = RpcClient::new_with_commitment(rpc_url, CommitmentConfig::confirmed());

    println!("loading sol balance...");
    let balance = if let Ok(balance) = rpc_client.get_balance(&wallet.pubkey()).await {
        balance
    } else {
        return Err("Failed to load balance".into());
    };

    info!("Balance: {:.2}", balance as f64 / LAMPORTS_PER_SOL as f64);

    if balance < 1_000_000 {
        return Err("Sol balance is too low!".into());
    }

    let mut proof = if let Ok(loaded_proof) = get_proof(&rpc_client, wallet.pubkey()).await {
        loaded_proof
    } else {
        info!("Failed to load proof.");
        info!("Creating proof account...");

        let ix = get_register_ix(wallet.pubkey());

        if let Ok((hash, _slot)) = rpc_client
            .get_latest_blockhash_with_commitment(rpc_client.commitment())
            .await
        {
            let mut tx = Transaction::new_with_payer(&[ix], Some(&wallet.pubkey()));

            tx.sign(&[&wallet], hash);

            let result = rpc_client
                .send_and_confirm_transaction_with_spinner_and_commitment(
                    &tx,
                    rpc_client.commitment(),
                )
                .await;

            if let Ok(sig) = result {
                println!("Sig: {}", sig.to_string());
            } else {
                return Err("Failed to create proof account".into());
            }
        }
        let proof = if let Ok(loaded_proof) = get_proof(&rpc_client, wallet.pubkey()).await {
            loaded_proof
        } else {
            return Err("Failed to get newly created proof".into());
        };
        proof
    };

    let epoch_hashes = Arc::new(RwLock::new(EpochHashes {
        best_hash: BestHash {
            solution: None,
            difficulty: 0,
        },
        submissions: HashMap::new(),
    }));

    let config = Arc::new(Mutex::new(Config {
        password,
        whitelist,
        // pool_id: db_pool.id,
    }));

    let wallet_extension = Arc::new(wallet);
    let proof_ext = Arc::new(Mutex::new(proof));
    let nonce_ext = Arc::new(Mutex::new(0u64));

    let client_nonce_ranges = Arc::new(RwLock::new(HashMap::new()));

    let shared_state = Arc::new(RwLock::new(AppState {
        sockets: HashMap::new(),
        miner_sockets: HashMap::new(),
    }));
    let ready_clients = Arc::new(Mutex::new(HashSet::new()));

    let app_wallet = wallet_extension.clone();
    let app_proof = proof_ext.clone();
    // Establish webocket connection for tracking pool proof changes.
    tokio::spawn(async move {
        proof_tracking_system(rpc_ws_url, app_wallet, app_proof).await;
    });

    let (client_message_sender, client_message_receiver) =
        tokio::sync::mpsc::unbounded_channel::<ClientMessage>();

    // Handle client messages
    let app_shared_state = shared_state.clone();
    let app_ready_clients = ready_clients.clone();
    let app_proof = proof_ext.clone();
    let app_epoch_hashes = epoch_hashes.clone();
    let app_app_database = app_database.clone();
    let app_virtual_database = virtual_database.clone();
    let app_client_nonce_ranges = client_nonce_ranges.clone();
    let app_powered_by_dbms = powered_by_dbms;
    tokio::spawn(async move {
        client_message_handler_system(
            client_message_receiver,
            &app_shared_state,
            app_app_database,
            app_virtual_database,
            app_epoch_hashes,
            app_ready_clients,
            app_proof,
            app_client_nonce_ranges,
            *min_difficulty,
            app_powered_by_dbms,
        )
        .await;
    });

    // Handle ready clients
    let app_shared_state = shared_state.clone();
    let app_proof = proof_ext.clone();
    let app_nonce = nonce_ext.clone();
    let app_epoch_hashes = epoch_hashes.clone();
    let app_app_database = app_database.clone();

    let app_client_nonce_ranges = client_nonce_ranges.clone();

    tokio::spawn(async move {
        let app_database = app_app_database;
        loop {
            let mut clients = Vec::new();
            {
                let ready_clients_lock = ready_clients.lock().await;
                for ready_client in ready_clients_lock.iter() {
                    clients.push(ready_client.clone());
                }
            };

            // MI: proof => current_proof
            let current_proof = { app_proof.lock().await.clone() };

            // let cutoff = get_cutoff(proof, 5);
            let cutoff = get_cutoff(proof, *buffer_time);
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
                let challenge = current_proof.challenge;

                for client in clients {
                    let nonce_range = {
                        let mut nonce = app_nonce.lock().await;
                        let start = *nonce;
                        // max hashes possible in 60s for a single client
                        *nonce += 2_000_000;
                        let end = *nonce;
                        start..end
                    };
                    {
                        let shared_state = app_shared_state.read().await;
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

                        if let Some(sender) = shared_state.sockets.get(&client) {
                            let _ = sender
                                .1
                                .lock()
                                .await
                                .send(Message::Binary(bin_data.to_vec()))
                                .await;
                            let _ = ready_clients.lock().await.remove(&client);
                            let _ = app_client_nonce_ranges
                                .write()
                                .await
                                .insert(sender.0, nonce_range);
                        }
                    }
                }
            }

            // tokio::time::sleep(Duration::from_secs(5)).await;
            tokio::time::sleep(Duration::from_secs(*buffer_time)).await;
        }
    });

    let (mine_success_sender, mut mine_success_receiver) =
        tokio::sync::mpsc::unbounded_channel::<MessageInternalMineSuccess>();

    let rpc_client = Arc::new(rpc_client);
    let app_proof = proof_ext.clone();
    let app_epoch_hashes = epoch_hashes.clone();
    let app_wallet = wallet_extension.clone();
    let app_nonce = nonce_ext.clone();
    // let app_prio_fee = priority_fee.clone();
    let app_rpc_client = rpc_client.clone();
    let app_config = config.clone();
    let app_app_database = app_database.clone();
    tokio::spawn(async move {
        let rpc_client = app_rpc_client;
        let app_database = app_app_database;
        loop {
            let mut old_proof = { app_proof.lock().await.clone() };

            let cutoff = get_cutoff(old_proof, 0);
            if cutoff <= 0 {
                // process solutions
                let solution = { app_epoch_hashes.read().await.best_hash.solution.clone() };
                if let Some(solution) = solution {
                    let signer = app_wallet.clone();
                    let mut ixs = vec![];

                    // TODO: choose better cu limit
                    let cu_limit_ix = ComputeBudgetInstruction::set_compute_unit_limit(480000);
                    ixs.push(cu_limit_ix);

                    let mut fee_type: &str = "static";
                    let fee: u64 = if *dynamic_fee {
                        fee_type = "estimate";
                        match pfee::dynamic_fee(
                            &rpc_client,
                            (*dynamic_fee_url).clone(),
                            *priority_fee_cap,
                        )
                        .await
                        {
                            Ok(fee) => {
                                let mut prio_fee = fee;
                                // MI: calc uplimit of priority fee for precious fee difficulty, eg. diff > 27
                                {
                                    let solution_difficulty = solution.to_hash().difficulty();
                                    if solution_difficulty > *extra_fee_difficulty {
                                        prio_fee = if let Some(ref priority_fee_cap) =
                                            *priority_fee_cap
                                        {
                                            (*priority_fee_cap).min(
                                                prio_fee
                                                    .saturating_mul(
                                                        100u64.saturating_add(*extra_fee_percent),
                                                    )
                                                    .saturating_div(100),
                                            )
                                        } else {
                                            // No priority_fee set as cap, not exceed 300K
                                            300_000.min(
                                                prio_fee
                                                    .saturating_mul(
                                                        100u64.saturating_add(*extra_fee_percent),
                                                    )
                                                    .saturating_div(100),
                                            )
                                        }
                                    }
                                }
                                prio_fee
                            }
                            Err(err) => {
                                let fee = priority_fee.unwrap_or(0);
                                println!(
                                    "Error: {} Falling back to static value: {} microlamports",
                                    err, fee
                                );
                                fee
                            }
                        }
                    } else {
                        // static
                        priority_fee.unwrap_or(0)
                    };

                    let prio_fee_ix = ComputeBudgetInstruction::set_compute_unit_price(fee);
                    ixs.push(prio_fee_ix);

                    let noop_ix = get_auth_ix(signer.pubkey());
                    ixs.push(noop_ix);

                    let mut bus = rand::thread_rng().gen_range(0..BUS_COUNT);
                    if let Ok((l_proof, (best_bus_id, _best_bus))) =
                        get_proof_and_best_bus(&rpc_client, signer.pubkey()).await
                    {
                        let _now = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .expect("Time went backwards")
                            .as_secs();

                        proof = l_proof;
                        bus = best_bus_id;
                    }

                    let ix_mine = get_mine_ix(signer.pubkey(), solution, bus);
                    ixs.push(ix_mine);

                    let difficulty = solution.to_hash().difficulty();
                    info!(
                        "Starting mine submission attempts with difficulty {}.",
                        difficulty
                    );

                    for i in 0..5 {
                        if let Ok((hash, _slot)) = rpc_client
                            .get_latest_blockhash_with_commitment(rpc_client.commitment())
                            .await
                        {
                            let mut tx = Transaction::new_with_payer(&ixs, Some(&signer.pubkey()));

                            tx.sign(&[&signer], hash);
                            info!(
                                "Sending signed tx... with {} priority fee {}",
                                fee_type, fee
                            );
                            info!("attempt: {}", i + 1);
                            let sig = rpc_client
                                .send_and_confirm_transaction_with_spinner(&tx)
                                .await;
                            if let Ok(sig) = sig {
                                // success
                                info!("Success!!");
                                info!("Sig: {}", sig);
                                if !*no_sound_notification {
                                    utils::play_sound();
                                }

                                if powered_by_dbms {
                                    // let itxn = InsertTxn {
                                    //     txn_type: "mine".to_string(),
                                    //     signature: sig.to_string(),
                                    //     priority_fee: prio_fee as u32,
                                    // };
                                    // let _ = app_database.add_new_txn(itxn).await.unwrap();
                                }

                                // update proof
                                loop {
                                    info!("Waiting for proof hash update");
                                    let latest_proof = { app_proof.lock().await.clone() };

                                    if old_proof.challenge.eq(&latest_proof.challenge) {
                                        info!("Proof challenge not updated yet..");
                                        old_proof = latest_proof;
                                        tokio::time::sleep(Duration::from_millis(1000)).await;
                                        continue;
                                    } else {
                                        info!("Proof challenge updated! Checking rewards earned.");
                                        let balance = (latest_proof.balance as f64)
                                            / 10f64.powf(ORE_TOKEN_DECIMALS as f64);
                                        info!("New balance: {}", balance);
                                        let rewards = latest_proof.balance - old_proof.balance;
                                        let dec_rewards = (rewards as f64)
                                            / 10f64.powf(ORE_TOKEN_DECIMALS as f64);
                                        info!("Earned: {} ORE", dec_rewards);

                                        let submissions =
                                            { app_epoch_hashes.read().await.submissions.clone() };

                                        if powered_by_dbms {
                                            // let submission_id = app_database.get_submission_id_with_nonce(u64::from_le_bytes(solution.n)).await.unwrap();

                                            // let _ = app_database.update_challenge_rewards(old_proof.challenge.to_vec(), submission_id, rewards).await.unwrap();
                                            // let _ = app_database.update_pool_rewards(app_wallet.pubkey().to_string(), rewards).await.unwrap();

                                            // let submissions = app_database.get_all_submission_for_challenge(old_proof.challenge.to_vec()).await.unwrap();
                                        }

                                        let mut total_hashpower: u64 = 0;
                                        if powered_by_dbms {
                                            // for submission in submissions.iter() {
                                            //     let hashpower = MIN_HASHPOWER * 2u64.pow(submission.difficulty as u32 - MIN_DIFF);
                                            //     total_hashpower += hashpower;
                                            // }
                                        } else {
                                            for submission in submissions.iter() {
                                                total_hashpower += submission.1 .1
                                            }
                                        }

                                        let _ =
                                            mine_success_sender.send(MessageInternalMineSuccess {
                                                difficulty,
                                                total_balance: balance,
                                                // pool_id: app_config.pool_id,
                                                rewards,
                                                total_hashpower,
                                                submissions,
                                            });

                                        if powered_by_dbms {
                                            {
                                                // let new_challenge = InsertChallenge {
                                                //     pool_id: app_config.pool_id,
                                                //     challenge: latest_proof.challenge.to_vec(),
                                                //     rewards_earned: None,
                                                // };

                                                // info!("NEW CHALLENGE: {:?}", new_challenge);
                                                // let result = app_database.add_new_challenge(new_challenge).await;

                                                // match result {
                                                //     Ok(_) => {}
                                                //     Err(AppDatabaseError::FailedToInsertNewEntity) => {
                                                //         panic!("Failed to create new challenge in database");
                                                //     },
                                                //     Err(_) => {
                                                //         panic!("AppDatabase query failed");
                                                //     }
                                                // }

                                                // {
                                                //     let mut prio_fee = app_prio_fee.lock().await;
                                                //     let mut decrease_amount = 0;
                                                //     if *prio_fee >=  1_000 {
                                                //         decrease_amount = 1_000;
                                                //     }
                                                //     if *prio_fee >=  50_000 {
                                                //         decrease_amount = 5_000;
                                                //     }
                                                //     if *prio_fee >=  100_000 {
                                                //         decrease_amount = 10_000;
                                                //     }

                                                //     *prio_fee = prio_fee.saturating_sub(decrease_amount);
                                                // }
                                            }
                                        } else {
                                            {
                                                let mut mut_proof = app_proof.lock().await;
                                                *mut_proof = latest_proof;
                                                break;
                                            }
                                        }

                                        // reset nonce
                                        {
                                            let mut nonce = app_nonce.lock().await;
                                            *nonce = 0;
                                        }
                                        // reset epoch hashes
                                        {
                                            info!("reset epoch hashes");
                                            let mut mut_epoch_hashes =
                                                app_epoch_hashes.write().await;
                                            mut_epoch_hashes.best_hash.solution = None;
                                            mut_epoch_hashes.best_hash.difficulty = 0;
                                            mut_epoch_hashes.submissions = HashMap::new();
                                        }
                                        break;
                                    }
                                }

                                break;
                            } else {
                                // sent error
                                if i >= 4 {
                                    warn!("Failed to send after 5 attempts. Discarding and refreshing data.");
                                    // reset nonce
                                    {
                                        let mut nonce = app_nonce.lock().await;
                                        *nonce = 0;
                                    }
                                    // reset epoch hashes
                                    {
                                        info!("reset epoch hashes");
                                        let mut mut_epoch_hashes = app_epoch_hashes.write().await;
                                        mut_epoch_hashes.best_hash.solution = None;
                                        mut_epoch_hashes.best_hash.difficulty = 0;
                                        mut_epoch_hashes.submissions = HashMap::new();
                                    }
                                    // break for (0..3), re-enter loop to restart
                                    break;
                                }
                            }
                            tokio::time::sleep(Duration::from_millis(1000)).await;
                        } else {
                            error!("Failed to get latest blockhash. retrying...");
                            tokio::time::sleep(Duration::from_millis(1000)).await;
                        }
                    }
                } else {
                    // error!("No best solution received yet.");
                    tokio::time::sleep(Duration::from_millis(1000)).await;
                }
            } else {
                tokio::time::sleep(Duration::from_secs(cutoff as u64)).await;
            };
        }
    });

    let app_shared_state = shared_state.clone();
    let app_app_database = app_database.clone();
    let app_rpc_client = rpc_client.clone();
    let app_wallet = wallet_extension.clone();
    // let app_powered_by_dbms = powered_by_dbms.clone();
    tokio::spawn(async move {
        let app_database = app_app_database;
        loop {
            while let Some(msg) = mine_success_receiver.recv().await {
                {
                    let decimals = 10f64.powf(ORE_TOKEN_DECIMALS as f64);
                    let pool_rewards_dec = (msg.rewards as f64).div(decimals);
                    let shared_state = app_shared_state.read().await;
                    for (_socket_addr, socket_sender) in shared_state.sockets.iter() {
                        let pubkey = socket_sender.0;

                        if let Some((supplied_diff, pubkey_hashpower)) =
                            msg.submissions.get(&pubkey)
                        {
                            // let hashpower_percent =
                            //     (*pubkey_hashpower as f64).div(msg.total_hashpower as f64);

                            let hashpower =
                                MIN_HASHPOWER * 2u64.pow(*supplied_diff as u32 - MIN_DIFF);

                            let hashpower_percent = (hashpower as u128)
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

                            if powered_by_dbms {
                                // let _ = app_database
                                //     .update_miner_reward(submission.miner_id, earned_rewards as u64)
                                //     .await
                                //     .unwrap();
                                // let earning = InsertEarning {
                                //     miner_id: submission.miner_id,
                                //     pool_id: msg.pool_id,
                                //     challenge_id: submission.challenge_id,
                                //     amount: earned_rewards,
                                // };
                                // let _ = app_database.add_new_earning(earning).await.unwrap();
                            }

                            if let Some(socket_sender) = shared_state.miner_sockets.get(&pubkey) {
                                let earned_rewards_dec = (earned_rewards as f64).div(decimals);

                                let message = format!(
                                    "Submitted Difficulty: {}\nPool Earned: {} ORE.\nPool Balance: {}\nMiner Earned: {} ORE for difficulty: {}",
                                    msg.difficulty,
                                    pool_rewards_dec,
                                    msg.total_balance,
                                    earned_rewards_dec,
                                    supplied_diff
                                );
                                if let Ok(_) = socket_sender
                                    .lock()
                                    .await
                                    .send(Message::Text(message))
                                    .await
                                {
                                } else {
                                    println!("Failed to send client text");
                                }
                            }
                        }
                    }
                }
                if let Ok(balance) = app_rpc_client.get_balance(&app_wallet.pubkey()).await {
                    info!(
                        "Sol Balance: {:.2}",
                        balance as f64 / LAMPORTS_PER_SOL as f64
                    );
                } else {
                    error!("Failed to load balance");
                };
            }
        }
    });

    let client_channel = client_message_sender.clone();
    let app_shared_state = shared_state.clone();
    let app = Router::new()
        .route("/", get(ws_handler))
        .route("/latest-blockhash", get(get_latest_blockhash))
        .route("/pool/authority/pubkey", get(get_pool_authority_pubkey))
        .route("/signup", post(post_signup))
        .route("/claim", post(post_claim))
        .route("/miner/rewards", get(get_miner_rewards))
        .route("/miner/balance", get(get_miner_balance))
        .with_state(app_shared_state)
        .layer(Extension(app_database))
        .layer(Extension(config))
        .layer(Extension(wallet_extension))
        .layer(Extension(client_channel))
        .layer(Extension(rpc_client))
        .layer(Extension(client_nonce_ranges))
        // Logging
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::default().include_headers(true)),
        );

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();

    tracing::debug!("listening on {}", listener.local_addr().unwrap());

    let app_shared_state = shared_state.clone();
    tokio::spawn(async move {
        ping_check_system(&app_shared_state).await;
    });

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .unwrap();

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
struct SignupParams {
    pubkey: String,
}

async fn post_signup(
    query_params: Query<SignupParams>,
    Extension(app_database): Extension<Arc<AppDatabase>>,
    Extension(rpc_client): Extension<Arc<RpcClient>>,
    Extension(wallet): Extension<Arc<Keypair>>,
    Extension(app_config): Extension<Arc<Config>>,
    body: String,
) -> impl IntoResponse {
    if let Ok(user_pubkey) = Pubkey::from_str(&query_params.pubkey) {
        let db_miner = app_database
            .get_miner_by_pubkey_str(user_pubkey.to_string())
            .await;

        match db_miner {
            Ok(miner) => {
                if miner.enabled {
                    println!("Miner account already enabled!");
                    return Response::builder()
                        .status(StatusCode::OK)
                        .header("Content-Type", "text/text")
                        .body("EXISTS".to_string())
                        .unwrap();
                }
            }
            Err(AppDatabaseError::EntityDoesNotExist) => {
                println!("No miner account exists. Signing up new user.");
            }
            Err(_) => {
                error!("Failed to get database pool connection");
                return Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .body("Failed to get db pool connection".to_string())
                    .unwrap();
            }
        }

        if let Some(whitelist) = &app_config.whitelist {
            if whitelist.contains(&user_pubkey) {
                let result = app_database
                    .add_new_miner(user_pubkey.to_string(), true)
                    .await;
                let miner = app_database
                    .get_miner_by_pubkey_str(user_pubkey.to_string())
                    .await
                    .unwrap();

                let wallet_pubkey = wallet.pubkey();
                let pool = app_database
                    .get_pool_by_authority_pubkey(wallet_pubkey.to_string())
                    .await
                    .unwrap();

                if result.is_ok() {
                    let new_reward = InsertReward {
                        miner_id: miner.id,
                        pool_id: pool.id,
                    };
                    let result = app_database.add_new_reward(new_reward).await;

                    if result.is_ok() {
                        return Response::builder()
                            .status(StatusCode::OK)
                            .header("Content-Type", "text/text")
                            .body("SUCCESS".to_string())
                            .unwrap();
                    } else {
                        error!("Failed to add miner rewards tracker to database");
                        return Response::builder()
                            .status(StatusCode::INTERNAL_SERVER_ERROR)
                            .body("Failed to add miner rewards tracker to database".to_string())
                            .unwrap();
                    }
                } else {
                    error!("Failed to add miner to database");
                    return Response::builder()
                        .status(StatusCode::INTERNAL_SERVER_ERROR)
                        .body("Failed to add miner to database".to_string())
                        .unwrap();
                }
            }
        }

        let serialized_tx = BASE64_STANDARD.decode(body.clone()).unwrap();
        let tx: Transaction = if let Ok(tx) = bincode::deserialize(&serialized_tx) {
            tx
        } else {
            error!("Failed to deserialize tx");
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body("Invalid Tx".to_string())
                .unwrap();
        };

        if !tx.is_signed() {
            error!("Tx missing signer");
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body("Invalid Tx".to_string())
                .unwrap();
        }

        let ixs = tx.message.instructions.clone();

        if ixs.len() > 1 {
            error!("Too many instructions");
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body("Invalid Tx".to_string())
                .unwrap();
        }

        let base_ix = system_instruction::transfer(&user_pubkey, &wallet.pubkey(), 1_000_000);
        let mut accts = Vec::new();
        for account_index in ixs[0].accounts.clone() {
            accts.push(tx.key(0, account_index.into()));
        }

        if accts.len() != 2 {
            error!("too many accts");
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body("Invalid Tx".to_string())
                .unwrap();
        }

        if ixs[0].data.ne(&base_ix.data) {
            error!("data missmatch");
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body("Invalid Tx".to_string())
                .unwrap();
        } else {
            println!("Valid signup tx, submitting.");

            if let Ok(_sig) = rpc_client.send_and_confirm_transaction(&tx).await {
                let res = app_database
                    .add_new_miner(user_pubkey.to_string(), true)
                    .await;
                let miner = app_database
                    .get_miner_by_pubkey_str(user_pubkey.to_string())
                    .await
                    .unwrap();

                let wallet_pubkey = wallet.pubkey();
                let pool = app_database
                    .get_pool_by_authority_pubkey(wallet_pubkey.to_string())
                    .await
                    .unwrap();

                if res.is_ok() {
                    let new_reward = InsertReward {
                        miner_id: miner.id,
                        pool_id: pool.id,
                    };
                    let result = app_database.add_new_reward(new_reward).await;

                    if result.is_ok() {
                        return Response::builder()
                            .status(StatusCode::OK)
                            .header("Content-Type", "text/text")
                            .body("SUCCESS".to_string())
                            .unwrap();
                    } else {
                        error!("Failed to add miner rewards tracker to database");
                        return Response::builder()
                            .status(StatusCode::INTERNAL_SERVER_ERROR)
                            .body("Failed to add miner rewards tracker to database".to_string())
                            .unwrap();
                    }
                } else {
                    error!("Failed to add miner to database");
                    return Response::builder()
                        .status(StatusCode::INTERNAL_SERVER_ERROR)
                        .body("Failed to add user to database".to_string())
                        .unwrap();
                }
            } else {
                return Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .body("Failed to send tx".to_string())
                    .unwrap();
            }
        }
    } else {
        error!("Signup with invalid pubkey");
        return Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body("Invalid Pubkey".to_string())
            .unwrap();
    }
}

#[derive(Deserialize)]
struct PubkeyParam {
    pubkey: String,
}

async fn get_miner_rewards(
    query_params: Query<PubkeyParam>,
    Extension(app_database): Extension<Arc<AppDatabase>>,
) -> impl IntoResponse {
    if let Ok(user_pubkey) = Pubkey::from_str(&query_params.pubkey) {
        let res = app_database
            .get_miner_rewards(user_pubkey.to_string())
            .await;

        match res {
            Ok(rewards) => {
                let decimal_bal =
                    rewards.balance as f64 / 10f64.powf(ore_api::consts::TOKEN_DECIMALS as f64);
                let response = format!("{}", decimal_bal);
                return Response::builder()
                    .status(StatusCode::OK)
                    .body(response)
                    .unwrap();
            }
            Err(_) => {
                return Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .body("Failed to get balance".to_string())
                    .unwrap();
            }
        }
    } else {
        return Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body("Invalid public key".to_string())
            .unwrap();
    }
}

async fn get_miner_balance(
    query_params: Query<PubkeyParam>,
    Extension(rpc_client): Extension<Arc<RpcClient>>,
) -> impl IntoResponse {
    if let Ok(user_pubkey) = Pubkey::from_str(&query_params.pubkey) {
        let miner_token_account = get_associated_token_address(&user_pubkey, &get_ore_mint());
        if let Ok(response) = rpc_client
            .get_token_account_balance(&miner_token_account)
            .await
        {
            return Response::builder()
                .status(StatusCode::OK)
                .body(response.ui_amount_string)
                .unwrap();
        } else {
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body("Failed to get token account balance".to_string())
                .unwrap();
        }
    } else {
        return Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body("Invalid public key".to_string())
            .unwrap();
    }
}

#[derive(Deserialize)]
struct ClaimParams {
    pubkey: String,
    amount: u64,
}

async fn post_claim(
    query_params: Query<ClaimParams>,
    Extension(app_database): Extension<Arc<AppDatabase>>,
    Extension(rpc_client): Extension<Arc<RpcClient>>,
    Extension(wallet): Extension<Arc<Keypair>>,
) -> impl IntoResponse {
    if let Ok(user_pubkey) = Pubkey::from_str(&query_params.pubkey) {
        let amount = query_params.amount;
        if let Ok(miner_rewards) = app_database
            .get_miner_rewards(user_pubkey.to_string())
            .await
        {
            if amount > miner_rewards.balance {
                return Response::builder()
                    .status(StatusCode::BAD_REQUEST)
                    .body("claim amount exceeds miner rewards balance".to_string())
                    .unwrap();
            }

            let ore_mint = get_ore_mint();
            let miner_token_account = get_associated_token_address(&user_pubkey, &ore_mint);

            let mut ixs = Vec::new();
            // let prio_fee_ix = ComputeBudgetInstruction::set_compute_unit_price(10000);
            // ixs.push(prio_fee_ix);
            if let Ok(response) = rpc_client
                .get_token_account_balance(&miner_token_account)
                .await
            {
                if let Some(_amount) = response.ui_amount {
                    info!("miner has valid token account.");
                } else {
                    info!("will create token account for miner");
                    ixs.push(
                        spl_associated_token_account::instruction::create_associated_token_account(
                            &wallet.pubkey(),
                            &user_pubkey,
                            &ore_api::consts::MINT_ADDRESS,
                            &spl_token::id(),
                        ),
                    )
                }
            } else {
                info!("Adding create ata ix for miner claim");
                ixs.push(
                    spl_associated_token_account::instruction::create_associated_token_account(
                        &wallet.pubkey(),
                        &user_pubkey,
                        &ore_api::consts::MINT_ADDRESS,
                        &spl_token::id(),
                    ),
                )
            }

            let ix = ore_api::instruction::claim(wallet.pubkey(), miner_token_account, amount);
            ixs.push(ix);

            if let Ok((hash, _slot)) = rpc_client
                .get_latest_blockhash_with_commitment(rpc_client.commitment())
                .await
            {
                let mut tx = Transaction::new_with_payer(&ixs, Some(&wallet.pubkey()));

                tx.sign(&[&wallet], hash);

                let result = rpc_client
                    .send_and_confirm_transaction_with_spinner_and_commitment(
                        &tx,
                        rpc_client.commitment(),
                    )
                    .await;
                match result {
                    Ok(sig) => {
                        info!("Miner successfully claimed.\nSig: {}", sig.to_string());

                        // TODO: use transacions, or at least put them into one query
                        let miner = app_database
                            .get_miner_by_pubkey_str(user_pubkey.to_string())
                            .await
                            .unwrap();
                        let db_pool = app_database
                            .get_pool_by_authority_pubkey(wallet.pubkey().to_string())
                            .await
                            .unwrap();
                        let _ = app_database
                            .decrease_miner_reward(miner.id, amount)
                            .await
                            .unwrap();
                        let _ = app_database
                            .update_pool_claimed(wallet.pubkey().to_string(), amount)
                            .await
                            .unwrap();

                        let itxn = InsertTxn {
                            txn_type: "claim".to_string(),
                            signature: sig.to_string(),
                            priority_fee: 0,
                        };
                        let _ = app_database.add_new_txn(itxn).await.unwrap();

                        let ntxn = app_database.get_txn_by_sig(sig.to_string()).await.unwrap();

                        let iclaim = InsertClaim {
                            miner_id: miner.id,
                            pool_id: db_pool.id,
                            txn_id: ntxn.id,
                            amount,
                        };
                        let _ = app_database.add_new_claim(iclaim).await.unwrap();

                        return Response::builder()
                            .status(StatusCode::OK)
                            .body("SUCCESS".to_string())
                            .unwrap();
                    }
                    Err(e) => {
                        println!("ERROR: {:?}", e);
                        return Response::builder()
                            .status(StatusCode::INTERNAL_SERVER_ERROR)
                            .body("FAILED".to_string())
                            .unwrap();
                    }
                }
            } else {
                return Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .body("FAILED".to_string())
                    .unwrap();
            }
        } else {
            return Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body("failed to get miner account from database".to_string())
                .unwrap();
        }
    } else {
        error!("Claim with invalid pubkey");
        return Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body("Invalid Pubkey".to_string())
            .unwrap();
    }
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
    // Extension(app_config): Extension<Arc<Config>>,
    Extension(client_channel): Extension<UnboundedSender<ClientMessage>>,
    Extension(app_database): Extension<Arc<AppDatabase>>,
    query_params: Query<WsQueryParams>,
) -> impl IntoResponse {
    let msg_timestamp = query_params.timestamp;

    let pubkey = auth_header.username();
    let signed_msg = auth_header.password();

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("Time went backwards")
        .as_secs();

    // Signed authentication message is only valid for 5 seconds
    if (now - query_params.timestamp) >= 5 {
        return Err((StatusCode::UNAUTHORIZED, "Timestamp too old."));
    }

    // verify client
    if false {
        // if let Ok(user_pubkey) = Pubkey::from_str(pubkey) {
        //     {
        //         let mut already_connected = false;
        //         for (_, (socket_pubkey, _)) in app_state.read().await.sockets.iter() {
        //             if user_pubkey == *socket_pubkey {
        //                 already_connected = true;
        //                 break;
        //             }
        //         }
        //         if already_connected {
        //             return Err((StatusCode::TOO_MANY_REQUESTS, "A client is already connected with that wallet"));
        //         }
        //     };

        //     let db_miner = app_database.get_miner_by_pubkey_str(pubkey.to_string()).await;

        //     let miner;
        //     match db_miner {
        //         Ok(db_miner) => {
        //             miner = db_miner;
        //         }
        //         Err(AppDatabaseError::EntityDoesNotExist) => {
        //             return Err((StatusCode::UNAUTHORIZED, "pubkey is not authorized to mine. please sign up."));
        //         },
        //         Err(_) => {
        //             error!("Failed to get database pool connection.");
        //             return Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error"));
        //         }
        //     }

        //     if !miner.enabled {
        //         return Err((StatusCode::UNAUTHORIZED, "pubkey is not authorized to mine"));
        //     }

        //     if let Ok(signature) = Signature::from_str(signed_msg) {
        //         let ts_msg = msg_timestamp.to_le_bytes();

        //         if signature.verify(&user_pubkey.to_bytes(), &ts_msg) {
        //             println!("Client: {addr} connected with pubkey {pubkey}.");
        //             return Ok(ws.on_upgrade(move |socket| handle_socket(socket, addr, user_pubkey, app_state, client_channel, miner.id)));
        //         } else {
        //             return Err((StatusCode::UNAUTHORIZED, "Sig verification failed"));
        //         }
        //     } else {
        //         return Err((StatusCode::UNAUTHORIZED, "Invalid signature"));
        //     }
        // } else {
        //     return Err((StatusCode::UNAUTHORIZED, "Invalid pubkey"));
        // }

        error!("Failed to get database pool connection, because dbms are not supported at present.");
        return Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error"));
    } else {
        // legacy approach
        if let Ok(pubkey) = Pubkey::from_str(pubkey) {
            // if let Some(whitelist) = &app_config.whitelist {
            //     if whitelist.contains(&pubkey) {
            //         if let Ok(signature) = Signature::from_str(signed_msg) {
            //             let ts_msg = msg_timestamp.to_le_bytes();

            //             if signature.verify(&pubkey.to_bytes(), &ts_msg) {
            //                 println!("Client: {addr} connected with pubkey {pubkey}.");
            //                 return Ok(ws.on_upgrade(move |socket| {
            //                     handle_socket(socket, addr, pubkey, app_state, client_channel)
            //                 }));
            //             } else {
            //                 return Err((StatusCode::UNAUTHORIZED, "Sig verification failed"));
            //             }
            //         } else {
            //             return Err((StatusCode::UNAUTHORIZED, "Invalid signature"));
            //         }
            //     } else {
            //         return Err((StatusCode::UNAUTHORIZED, "pubkey is not authorized to mine"));
            //     }
            // } else {
                if let Ok(signature) = Signature::from_str(signed_msg) {
                    let ts_msg = msg_timestamp.to_le_bytes();

                    if signature.verify(&pubkey.to_bytes(), &ts_msg) {
                        println!("Client: {addr} connected with pubkey {pubkey}.");
                        return Ok(ws.on_upgrade(move |socket| {
                            handle_socket(socket, addr, pubkey, app_state, client_channel)
                        }));
                    } else {
                        return Err((StatusCode::UNAUTHORIZED, "Sig verification failed"));
                    }
                } else {
                    return Err((StatusCode::UNAUTHORIZED, "Invalid signature"));
                }
            // }
        } else {
            return Err((StatusCode::UNAUTHORIZED, "Invalid pubkey"));
        }
    }
}

async fn handle_socket(
    mut socket: WebSocket,
    who: SocketAddr,
    who_pubkey: Pubkey,
    rw_app_state: Arc<RwLock<AppState>>,
    client_channel: UnboundedSender<ClientMessage>,
    // miner_id: i32,
) {
    if socket
        .send(axum::extract::ws::Message::Ping(vec![1, 2, 3]))
        .await
        .is_ok()
    {
        println!("Pinged {who}... pubkey: {who_pubkey}");
    } else {
        println!("could not ping {who} pubkey: {who_pubkey}");

        // if we can't ping we can't do anything, return to close the connection
        return;
    }

    let (sender, mut receiver) = socket.split();
    let mut app_state = rw_app_state.write().await;
    if app_state.sockets.contains_key(&who) {
        println!("Socket addr: {who} already has an active connection");
        return;
    } else {
        let sender = Arc::new(Mutex::new(sender));
        app_state.sockets.insert(who, (who_pubkey, sender.clone()));
        app_state
            .miner_sockets
            // .insert(miner_id, (who_pubkey, sender));
            .insert(who_pubkey, sender);
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
    app_state.miner_sockets.remove(&who_pubkey);
    drop(app_state);

    info!("Client: {} disconnected!", who_pubkey.to_string());
}

fn process_message(
    msg: Message,
    who: SocketAddr,
    client_channel: UnboundedSender<ClientMessage>,
) -> ControlFlow<(), ()> {
    match msg {
        Message::Text(t) => {
            println!(">>> {who} sent str: {t:?}");
        }
        Message::Binary(d) => {
            // first 8 bytes are message type
            let message_type = d[0];
            match message_type {
                0 => {
                    // println!("Got Ready message");
                    let mut b_index = 1;

                    let mut pubkey = [0u8; 32];
                    for i in 0..32 {
                        pubkey[i] = d[i + b_index];
                    }
                    b_index += 32;

                    let mut ts = [0u8; 8];
                    for i in 0..8 {
                        ts[i] = d[i + b_index];
                    }

                    let ts = u64::from_le_bytes(ts);

                    let now = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .expect("Time went backwards")
                        .as_secs();

                    let time_since = now - ts;
                    if time_since > 5 {
                        error!("Client tried to ready up with expired signed message");
                        return ControlFlow::Break(());
                    }

                    let msg = ClientMessage::Ready(who);
                    let _ = client_channel.send(msg);
                }
                1 => {
                    let msg = ClientMessage::Mining(who);
                    let _ = client_channel.send(msg);
                }
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
                }
                _ => {
                    println!(">>> {} sent an invalid message", who);
                }
            }
        }
        Message::Close(c) => {
            if let Some(cf) = c {
                println!(
                    ">>> {} sent close with code {} and reason `{}`",
                    who, cf.code, cf.reason
                );
            } else {
                println!(">>> {who} somehow sent close message without CloseFrame");
            }
            return ControlFlow::Break(());
        }
        Message::Pong(_v) => {
            //println!(">>> {who} sent pong with {v:?}");
        }
        Message::Ping(_v) => {
            //println!(">>> {who} sent ping with {v:?}");
        }
    }

    ControlFlow::Continue(())
}

async fn proof_tracking_system(ws_url: String, wallet: Arc<Keypair>, proof: Arc<Mutex<Proof>>) {
    loop {
        println!("Establishing rpc websocket connection...");
        let mut ps_client = PubsubClient::new(&ws_url).await;
        let mut attempts = 0;

        while ps_client.is_err() && attempts < 3 {
            error!("Failed to connect to websocket, retrying...");
            ps_client = PubsubClient::new(&ws_url).await;
            tokio::time::sleep(Duration::from_millis(1000)).await;
            attempts += 1;
        }
        info!("RPC WS connection established!");

        let app_wallet = wallet.clone();
        if let Ok(ps_client) = ps_client {
            // The `PubsubClient` must be `Arc`ed to share it across threads/tasks.
            let ps_client = Arc::new(ps_client);
            let app_proof = proof.clone();
            let account_pubkey = proof_pubkey(app_wallet.pubkey());
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

            info!("Subscribed tracking pool proof updates with websocket");
            if let Ok((mut account_sub_notifications, _account_unsub)) = pubsub {
                while let Some(response) = account_sub_notifications.next().await {
                    let data = response.value.data.decode();
                    if let Some(data_bytes) = data {
                        // if let Ok(bus) = Bus::try_from_bytes(&data_bytes) {
                        //     let _ = sender.send(AccountUpdatesData::BusData(*bus));
                        // }
                        // if let Ok(ore_config) = ore_api::state::Config::try_from_bytes(&data_bytes) {
                        //     let _ = sender.send(AccountUpdatesData::TreasuryConfigData(*ore_config));
                        // }
                        if let Ok(new_proof) = Proof::try_from_bytes(&data_bytes) {
                            // let _ = sender.send(AccountUpdatesData::ProofData(*proof));
                            //
                            {
                                let mut app_proof = app_proof.lock().await;
                                *app_proof = *new_proof;
                                drop(app_proof);
                            }
                        }
                    }
                }
            }
        }
    }
}

async fn client_message_handler_system(
    mut receiver_channel: UnboundedReceiver<ClientMessage>,
    shared_state: &Arc<RwLock<AppState>>,
    app_database: Arc<AppDatabase>,
    _virtual_database: Arc<DashMap<Pubkey, InsertSubmission>>,
    epoch_hashes: Arc<RwLock<EpochHashes>>,
    ready_clients: Arc<Mutex<HashSet<SocketAddr>>>,
    proof: Arc<Mutex<Proof>>,
    client_nonce_ranges: Arc<RwLock<HashMap<Pubkey, Range<u64>>>>,
    min_difficulty: u32,
    powered_by_dbms: bool,
) {
    while let Some(client_message) = receiver_channel.recv().await {
        match client_message {
            ClientMessage::Ready(addr) => {
                info!("Client {} is ready!", addr.to_string());
                {
                    let shared_state = shared_state.read().await;
                    if let Some(sender) = shared_state.sockets.get(&addr) {
                        {
                            let mut ready_clients = ready_clients.lock().await;
                            ready_clients.insert(addr);
                        }

                        if let Ok(_) = sender
                            .1
                            .lock()
                            .await
                            .send(Message::Text(String::from("Client successfully added.")))
                            .await
                        {
                        } else {
                            println!("Failed notify client they were readied up!");
                        }
                    }
                }
            }
            ClientMessage::Mining(addr) => {
                println!("Client {} has started mining!", addr.to_string());
            }
            ClientMessage::BestSolution(_addr, solution, pubkey) => {
                let pubkey_str = pubkey.to_string();
                // let challenge = app_database.get_latest_challenge().await.unwrap();
                // let challenge = virtual_database.get_latest_challenge().await.unwrap();
                let challenge = {
                    let proof = proof.lock().await;
                    proof.challenge
                };

                let nonce_range: Range<u64> = {
                    if let Some(nr) = client_nonce_ranges.read().await.get(&pubkey) {
                        nr.clone()
                    } else {
                        error!("Client nonce range not set!");
                        continue;
                    }
                };

                let nonce = u64::from_le_bytes(solution.n);

                if !nonce_range.contains(&nonce) {
                    error!("Client submitted nonce out of assigned range");
                    continue;
                }

                if solution.is_valid(&challenge) {
                    let diff = solution.to_hash().difficulty();
                    info!("{} found diff: {}", pubkey_str, diff);
                    // if diff >= MIN_DIFF {
                    if diff >= min_difficulty {
                        // calculate rewards
                        if powered_by_dbms {
                            info!("CHALLENGE: {:?}", challenge);

                            // let miner = virtual_database.get_miner_by_pubkey_str(pubkey_str).await.unwrap();

                            // let new_submission = InsertSubmission {
                            //     miner_id: miner.id,
                            //     challenge_id: challenge.id,
                            //     digest: Some(solution.d.to_vec()),
                            //     nonce,
                            //     difficulty: diff as i8,
                            // };

                            // info!("NEW SUBMISSION: {:?}", new_submission);
                            // let _ = app_database.add_new_submission(new_submission).await.unwrap();
                        } else {
                            // MI
                            // let hashpower = MIN_HASHPOWER * 2u64.pow(diff - min_difficulty);
                            let hashpower = MIN_HASHPOWER * 2u64.pow(diff - MIN_DIFF);

                            {
                                let mut epoch_hashes = epoch_hashes.write().await;
                                epoch_hashes.submissions.insert(pubkey, (diff, hashpower));
                                if diff > epoch_hashes.best_hash.difficulty {
                                    epoch_hashes.best_hash.difficulty = diff;
                                    epoch_hashes.best_hash.solution = Some(solution);
                                }
                            }
                        }
                    } else {
                        error!("Diff too low, skipping");
                    }
                } else {
                    error!(
                        "{} returned a solution which is invalid for the latest challenge!",
                        pubkey
                    );
                    // MI: return will stop this spawned thread and stop mining process
                    // return;
                }
            }
        }
    }
}

async fn ping_check_system(shared_state: &Arc<RwLock<AppState>>) {
    loop {
        // send ping to all sockets
        let mut failed_sockets = Vec::new();
        let app_state = shared_state.read().await;
        // I don't like doing all this work while holding this lock...
        for (who, socket) in app_state.sockets.iter() {
            if socket
                .1
                .lock()
                .await
                .send(Message::Ping(vec![1, 2, 3]))
                .await
                .is_ok()
            {
                //println!("Pinged: {who}...");
            } else {
                failed_sockets.push(who.clone());
            }
        }
        drop(app_state);

        // remove any sockets where ping failed
        let mut app_state = shared_state.write().await;
        for address in failed_sockets {
            app_state.sockets.remove(&address);
        }
        drop(app_state);

        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

fn styles() -> Styles {
    Styles::styled()
        .header(AnsiColor::Red.on_default() | Effects::BOLD)
        .usage(AnsiColor::Red.on_default() | Effects::BOLD)
        .literal(AnsiColor::Blue.on_default() | Effects::BOLD)
        .placeholder(AnsiColor::Green.on_default())
}
