use std::{
    collections::HashMap,
    str::FromStr,
    sync::{atomic::Ordering::Relaxed, Arc},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use base64::{prelude::BASE64_STANDARD, Engine};
use chrono::Local;
use ore_api::{consts::BUS_COUNT, error::OreError, state::Proof};
use rand::Rng;
use solana_client::{
    client_error::ClientErrorKind, nonblocking::rpc_client::RpcClient,
    rpc_config::RpcSendTransactionConfig,
    send_and_confirm_transactions_in_parallel::SendAndConfirmConfig,
};
use solana_sdk::{
    commitment_config::CommitmentLevel, compute_budget::ComputeBudgetInstruction, signer::Signer,
    transaction::Transaction,
};
use solana_transaction_status::UiTransactionEncoding;
use tokio::sync::{mpsc::UnboundedSender, Mutex, RwLock};
use tracing::{debug, error, info, warn};

use crate::{
    database::{Database, PoweredByDbms},
    get_messaging_flags, models,
    notification::RewardsMessage,
    pfee, tpu,
    utils::{
        self, get_auth_ix, get_clock, get_config_and_proof, get_cutoff, get_cutoff_with_risk,
        get_mine_ix, get_proof, get_reset_ix, ORE_TOKEN_DECIMALS,
    },
    AppState, EpochHashes, InsertTransaction, MessageInternalAllClients,
    MessageInternalMineSuccess, MessagingFlags, MineConfig, WalletExtension, CHECK_LIMIT,
    EPOCH_DURATION, NO_BEST_SOLUTION_INTERVAL, PAUSED, POWERED_BY_DBMS, RPC_RETRIES, SUBMIT_LIMIT,
};

pub async fn pool_submission_processor<'a>(
    app_rpc_client: Arc<RpcClient>,
    app_mine_config: Arc<MineConfig>,
    app_shared_state: Arc<RwLock<AppState>>,
    app_proof: Arc<Mutex<Proof>>,
    app_epoch_hashes: Arc<RwLock<EpochHashes>>,
    app_wallet: Arc<WalletExtension>,
    app_nonce: Arc<Mutex<u64>>,
    app_dynamic_fee: Arc<bool>,
    app_dynamic_fee_url: Arc<Option<String>>,
    app_priority_fee: Arc<Option<u64>>,
    app_priority_fee_cap: Arc<Option<u64>>,
    app_extra_fee_difficulty: Arc<u32>,
    app_extra_fee_percent: Arc<u64>,
    app_send_tpu_mine_tx: Arc<bool>,
    app_no_sound_notification: Arc<bool>,
    app_database: Arc<Database>,
    app_all_clients_sender: UnboundedSender<MessageInternalAllClients>,
    mine_success_sender: UnboundedSender<MessageInternalMineSuccess>,
    app_slack_message_sender: UnboundedSender<RewardsMessage>,
    app_discord_message_sender: UnboundedSender<RewardsMessage>,
    app_slack_difficulty: Arc<u32>,
    app_messaging_diff: Arc<u32>,
    app_buffer_time: Arc<u64>,
    app_risk_time: Arc<u64>,
    // app_messaging_flags: MessagingFlags,
) {
    let rpc_client = app_rpc_client;
    let mine_config = app_mine_config;
    let database = app_database;
    let send_tpu_mine_tx = app_send_tpu_mine_tx;
    let slack_message_sender = app_slack_message_sender;
    let discord_message_sender = app_discord_message_sender;
    let slack_difficulty = app_slack_difficulty;
    let messaging_diff = app_messaging_diff;
    // let messaging_flags = app_messaging_flags;
    let messaging_flags = get_messaging_flags();

    // let wallet_pubkey = app_wallet.clone().miner_wallet.clone().pubkey();
    let mut solution_is_none_counter = 0;
    let mut num_waiting = 0;
    loop {
        let old_proof: Proof;
        let mut lock = app_proof.try_lock();
        if let Ok(ref mut mutex) = lock {
            old_proof = mutex.clone();
        } else {
            warn!(target: "server_log", "app_proof.try_lock failed, will try again in a moment.");
            tokio::time::sleep(Duration::from_millis(1000)).await;
            continue;
        }
        drop(lock);

        let cutoff = if (*app_risk_time).gt(&0) {
            get_cutoff_with_risk(&rpc_client, old_proof, *app_buffer_time, *app_risk_time).await
        } else {
            get_cutoff(&rpc_client, old_proof, *app_buffer_time).await
        };
        debug!(target: "server_log", "Enter new loop iteration. Let's check current cutoff value: {cutoff}");
        if cutoff <= 0_i64 {
            if cutoff <= -(*app_buffer_time as i64) {
                // buffer time elapsed, prepare to process solution
                let reader = app_epoch_hashes.read().await;
                let solution = reader.best_hash.solution.clone();
                drop(reader);

                let shared_state_lock = app_shared_state.read().await;
                let num_active_miners = shared_state_lock.sockets.len();
                drop(shared_state_lock);

                // start to process solution
                if solution.is_some() {
                    let signer = app_wallet.clone().miner_wallet.clone();
                    let wallet_pubkey = signer.pubkey();

                    let bus = rand::thread_rng().gen_range(0..BUS_COUNT);

                    let mut success = false;
                    let reader = app_epoch_hashes.read().await;
                    let best_solution = reader.best_hash.solution.clone();
                    let contributions = reader.contributions.clone();
                    drop(reader);
                    let num_contributions = contributions.len();
                    // MI, wait until all active miners' submissions received, and waiting times
                    // < 6 checkpoints. if min_difficulty is relative high, may
                    // always false, num_contributions depends on min diff
                    if num_contributions != num_active_miners && num_waiting < 6 {
                        tokio::time::sleep(Duration::from_millis(500)).await;
                        num_waiting += 1;
                        continue;
                    } else {
                        // reset waiting times
                        num_waiting = 0;
                    }

                    // set mining pause flag before submitting best solution
                    info!(target: "server_log", "pause new mining mission for pool submission.");
                    PAUSED.store(true, Relaxed);

                    for i in 0..SUBMIT_LIMIT {
                        if let Some(best_solution) = best_solution {
                            let difficulty = best_solution.to_hash().difficulty();

                            info!(target: "server_log",
                                "Submitting attempt {} with ✨ diff {} ✨ of {} qualified contributions at {}.",
                                i + 1,
                                difficulty,
                                num_contributions,
                                Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
                            );
                            info!(target: "server_log",
                                "Submission Challenge: {}",
                                BASE64_STANDARD.encode(old_proof.challenge)
                            );

                            info!(target: "server_log", "Getting current/latest config and proof.");
                            let ore_config = if let Ok((loaded_config, loaded_proof)) =
                                get_config_and_proof(&rpc_client, signer.pubkey()).await
                            {
                                info!(target: "server_log",
                                    "Current/latest pool Challenge: {}",
                                    BASE64_STANDARD.encode(loaded_proof.challenge)
                                );

                                if !best_solution.is_valid(&loaded_proof.challenge) {
                                    error!(target: "server_log", "❌ SOLUTION IS NOT VALID ANYMORE!");
                                    // let mut lock = app_proof.lock().await;
                                    // *lock = loaded_proof;
                                    // drop(lock);
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
                                    info!(target: "server_log", "Including reset tx.");
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
                                            if best_solution_difficulty >= *app_extra_fee_difficulty
                                            {
                                                prio_fee = if let Some(ref app_priority_fee_cap) =
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
                                    },
                                    Err(err) => {
                                        let fee = app_priority_fee.unwrap_or(0);
                                        info!(target: "server_log",
                                            "Error: {} Falling back to static value: {} microlamports",
                                            err, fee
                                        );
                                        fee
                                    },
                                }
                            } else {
                                // MI: consider to pay more fee for precious diff even with
                                // static fee mode
                                let mut prio_fee = app_priority_fee.unwrap_or(0);
                                // MI: calc uplimit of priority fee for precious diff
                                {
                                    let best_solution_difficulty =
                                        best_solution.to_hash().difficulty();
                                    if best_solution_difficulty >= *app_extra_fee_difficulty {
                                        prio_fee = if let Some(ref app_priority_fee_cap) =
                                            *app_priority_fee_cap
                                        {
                                            (*app_priority_fee_cap).min(
                                                prio_fee
                                                    .saturating_mul(
                                                        100u64
                                                            .saturating_add(*app_extra_fee_percent),
                                                    )
                                                    .saturating_div(100),
                                            )
                                        } else {
                                            // No priority_fee_cap was set
                                            // not exceed 300K
                                            300_000.min(
                                                prio_fee
                                                    .saturating_mul(
                                                        100u64
                                                            .saturating_add(*app_extra_fee_percent),
                                                    )
                                                    .saturating_div(100),
                                            )
                                        }
                                    }
                                }
                                prio_fee
                            };

                            let prio_fee_ix = ComputeBudgetInstruction::set_compute_unit_price(fee);
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
                                info!(target: "server_log", "Send tpu mine tx flag is on.");
                                let config = SendAndConfirmConfig {
                                    resign_txs_count: Some(5),
                                    with_spinner: true,
                                };
                                info!(target: "server_log",
                                    "Sending tpu and confirm... with {} priority fee {}",
                                    fee_type, fee
                                );
                                match tpu::send_and_confirm(&rpc_client, &ixs, &*signer, config)
                                    .await
                                {
                                    Ok(_) => {
                                        success = true;
                                        info!(target: "server_log", "✅ Success!!");
                                    },
                                    Err(e) => {
                                        error!(target: "server_log", "Error occurred within tpu::send_and_confirm: {}", e)
                                    },
                                }
                            } else {
                                info!(target: "server_log", "Send tpu mine tx flag is off. Use RPC call instead.");
                                // vanilla rpc approach
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
                                    info!(target: "server_log",
                                        "Sending rpc signed tx... with {} priority fee {}",
                                        fee_type, fee
                                    );
                                    info!(target: "server_log", "attempt: {}", i + 1);
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
                                            info!(target: "server_log", "✅ Success!!");
                                            info!(target: "server_log", "Sig: {}", sig);

                                            let powered_by_dbms =
                                                POWERED_BY_DBMS.get_or_init(|| {
                                                    let key = "POWERED_BY_DBMS";
                                                    match std::env::var(key) {
                                                        Ok(val) => {
                                                            PoweredByDbms::from_str(&val).expect("POWERED_BY_DBMS must be set correctly.")
                                                        },
                                                        Err(_) => PoweredByDbms::Unavailable,
                                                    }
                                                });

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
                                                        error!(target: "server_log", "Failed to add tx record to db! Retrying...");
                                                        tokio::time::sleep(Duration::from_millis(
                                                            1000,
                                                        ))
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
                                                                        error!(target: "server_log", "Ore: The epoch has ended and needs reset. Retrying...");
                                                                        continue;
                                                                    }
                                                                    e if e == OreError::HashInvalid as u32 => {
                                                                        error!(target: "server_log", "❌ Ore: The provided hash is invalid. See you next solution.");

                                                                        // break for (0..SUBMIT_LIMIT), re-enter outer loop to restart
                                                                        break;
                                                                    }
                                                                    _ => {
                                                                        error!(target: "server_log", "{}", &err.to_string());
                                                                        continue;
                                                                    }
                                                                }
                                                            },

                                                            // Non custom instruction error, return
                                                            _ => {
                                                                error!(target: "server_log", "{}", &err.to_string());
                                                            }
                                                        }
                                                    }

                                                    // MI: other error like what?
                                                    _ => {
                                                        error!(target: "server_log", "{}", &err.to_string());
                                                    }
                                                }
                                            // TODO: is sleep here necessary?, MI
                                            tokio::time::sleep(Duration::from_millis(100)).await
                                        },
                                    }
                                } else {
                                    error!(target: "server_log", "Failed to get latest blockhash. retrying...");
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
                                let app_app_discord_message_sender = discord_message_sender.clone();
                                let app_app_slack_difficulty = slack_difficulty.clone();
                                let app_app_messaging_diff = messaging_diff.clone();
                                let app_app_proof = app_proof.clone();
                                let app_app_wallet = app_wallet.clone();
                                let app_app_epoch_hashes = app_epoch_hashes.clone();
                                tokio::spawn(async move {
                                    let mine_success_sender = app_app_mine_success_sender;
                                    let app_nonce = app_app_nonce;
                                    let database = app_app_database;
                                    let mine_config = app_app_config;
                                    let rpc_client = app_app_rpc_client;
                                    let slack_message_sender = app_app_slack_message_sender.clone();
                                    let discord_message_sender =
                                        app_app_discord_message_sender.clone();
                                    let slack_difficulty = app_app_slack_difficulty.clone();
                                    let messaging_diff = app_app_messaging_diff.clone();
                                    let app_proof = app_app_proof;
                                    let app_wallet = app_app_wallet;
                                    let app_epoch_hashes = app_app_epoch_hashes;

                                    // update proof
                                    // limit number of checking no more than CHECK_LIMIT
                                    let mut num_checking = 0;
                                    loop {
                                        info!(target: "server_log", "Waiting & Checking for proof challenge update");
                                        // Wait 500ms then check for updated proof
                                        tokio::time::sleep(Duration::from_millis(500)).await;
                                        let lock = app_proof.lock().await;
                                        let latest_proof = lock.clone();
                                        drop(lock);

                                        if old_proof.challenge.eq(&latest_proof.challenge) {
                                            info!(target: "server_log", "Proof challenge not updated yet..");
                                            num_checking += 1;
                                            if num_checking >= CHECK_LIMIT {
                                                warn!(target: "server_log", "No challenge update detected after {CHECK_LIMIT} checkpoints. No more waiting, just keep going...");
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
                                                info!(target: "server_log",
                                                    "OLD PROOF CHALLENGE: {}",
                                                    BASE64_STANDARD.encode(old_proof.challenge)
                                                );
                                                info!(target: "server_log",
                                                    "RPC PROOF CHALLENGE: {}",
                                                    BASE64_STANDARD.encode(p.challenge)
                                                );
                                                if old_proof.challenge.ne(&p.challenge) {
                                                    info!(target: "server_log", "Found new proof from rpc call rather than websocket...");
                                                    let mut lock = app_proof.lock().await;
                                                    *lock = p;
                                                    drop(lock);

                                                    info!(target: "server_log", "Checking rewards earned.");
                                                    let lock = app_proof.lock().await;
                                                    let latest_proof = lock.clone();
                                                    drop(lock);
                                                    let balance = (latest_proof.balance as f64)
                                                        / 10f64.powf(ORE_TOKEN_DECIMALS as f64);
                                                    info!(target: "server_log", "New balance: {}", balance);

                                                    let multiplier =
                                                        if let Some(config) = ore_config {
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
                                                    info!(target: "server_log", "Multiplier: {}", multiplier);

                                                    let rewards = (latest_proof.balance
                                                        - old_proof.balance)
                                                        as i64;
                                                    let dec_rewards = (rewards as f64)
                                                        / 10f64.powf(ORE_TOKEN_DECIMALS as f64);
                                                    info!(target: "server_log", "Earned: {} ORE", dec_rewards);

                                                    // reset nonce and epoch_hashes
                                                    info!(target: "server_log", "reset nonce and epoch hashes");
                                                    // reset nonce
                                                    {
                                                        let mut nonce = app_nonce.lock().await;
                                                        *nonce = 0;
                                                    }
                                                    // reset epoch hashes
                                                    {
                                                        let mut mut_epoch_hashes =
                                                            app_epoch_hashes.write().await;
                                                        mut_epoch_hashes.challenge = p.challenge;
                                                        mut_epoch_hashes.best_hash.solution = None;
                                                        mut_epoch_hashes.best_hash.difficulty = 0;
                                                        mut_epoch_hashes.contributions =
                                                            HashMap::new();
                                                    }

                                                    // unset mining pause flag to start new mining mission
                                                    info!(target: "server_log", "resume new mining mission");
                                                    PAUSED.store(false, Relaxed);

                                                    // Mission completed, send signal to tx sender
                                                    let _ = mission_completed_sender.send(0);

                                                    let powered_by_dbms =
                                                        POWERED_BY_DBMS.get_or_init(|| {
                                                        let key = "POWERED_BY_DBMS";
                                                        match std::env::var(key) {
                                                            Ok(val) => {
                                                                PoweredByDbms::from_str(&val).expect("POWERED_BY_DBMS must be set correctly.")
                                                            },
                                                            Err(_) => PoweredByDbms::Unavailable,
                                                        }
                                                    });
                                                    if powered_by_dbms == &PoweredByDbms::Sqlite {
                                                        info!(target: "server_log", "Adding new challenge record to db");
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
                                                            error!(target: "server_log", "Failed to add new challenge record to db.");
                                                            info!(target: "server_log", "Check if the new challenge already exists in db.");
                                                            if let Ok(_) = database
                                                                .get_challenge_by_challenge(
                                                                    new_challenge.challenge.clone(),
                                                                )
                                                                .await
                                                            {
                                                                info!(target: "server_log", "Challenge already exists, continuing");
                                                                break;
                                                            }

                                                            tokio::time::sleep(
                                                                Duration::from_millis(1000),
                                                            )
                                                            .await;
                                                        }
                                                        info!(target: "server_log", "New challenge record successfully added to db");
                                                    }

                                                    break;
                                                }
                                            } else {
                                                error!(target: "server_log", "Failed to get proof via rpc call.");
                                            }
                                        } else {
                                            info!(target: "server_log", "Proof challenge updated!");
                                            let mut submission_challenge_id = i64::MAX;
                                            let powered_by_dbms =
                                                POWERED_BY_DBMS.get_or_init(|| {
                                                    let key = "POWERED_BY_DBMS";
                                                    match std::env::var(key) {
                                                        Ok(val) => {
                                                            PoweredByDbms::from_str(&val).expect("POWERED_BY_DBMS must be set correctly.")
                                                        },
                                                        Err(_) => PoweredByDbms::Unavailable,
                                                    }
                                                });
                                            if powered_by_dbms == &PoweredByDbms::Sqlite {
                                                // Fetch old proof challenge(id used later) records from db
                                                info!(target: "server_log", "Check if old/last challenge for the pool exists in the database");
                                                let old_challenge;
                                                loop {
                                                    if let Ok(c) = database
                                                        .get_challenge_by_challenge(
                                                            old_proof.challenge.to_vec(),
                                                        )
                                                        .await
                                                    {
                                                        old_challenge = c;
                                                        submission_challenge_id = old_challenge.id;
                                                        break;
                                                    } else {
                                                        warn!(target: "server_log",
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
                                                            error!(target: "server_log", "Failed to add old/last challenge record to db.");
                                                            info!(target: "server_log", "Check if the challenge already exists in db.");
                                                            if let Ok(_) = database
                                                                .get_challenge_by_challenge(
                                                                    new_challenge.challenge.clone(),
                                                                )
                                                                .await
                                                            {
                                                                info!(target: "server_log", "The challenge already exists, continuing");
                                                                break;
                                                            }

                                                            tokio::time::sleep(
                                                                Duration::from_millis(1_000),
                                                            )
                                                            .await;
                                                        }
                                                        info!(target: "server_log", "Old/last challenge record successfully added to db");
                                                        // tokio::time::sleep(Duration::from_millis(1_000)).await;
                                                    }
                                                }

                                                // Add new challenge record to db
                                                info!(target: "server_log", "Adding new challenge record to db");
                                                let new_challenge = models::InsertChallenge {
                                                    pool_id: mine_config.pool_id,
                                                    challenge: latest_proof.challenge.to_vec(),
                                                    rewards_earned: None,
                                                };

                                                while let Err(_) = database
                                                    .add_new_challenge(new_challenge.clone())
                                                    .await
                                                {
                                                    error!(target: "server_log",
                                                        "Failed to add new challenge record to db."
                                                    );
                                                    info!(target: "server_log", "Check if new challenge already exists in db.");
                                                    if let Ok(_) = database
                                                        .get_challenge_by_challenge(
                                                            new_challenge.challenge.clone(),
                                                        )
                                                        .await
                                                    {
                                                        info!(target: "server_log",
                                                            "Challenge already exists in db, continuing"
                                                        );
                                                        break;
                                                    }

                                                    tokio::time::sleep(Duration::from_millis(
                                                        1_000,
                                                    ))
                                                    .await;
                                                }
                                                info!(target: "server_log",
                                                    "New challenge record successfully added to db"
                                                );
                                            }

                                            info!(target: "server_log", "Checking rewards earned.");
                                            let lock = app_proof.lock().await;
                                            let latest_proof = lock.clone();
                                            drop(lock);
                                            let balance = (latest_proof.balance as f64)
                                                / 10f64.powf(ORE_TOKEN_DECIMALS as f64);
                                            info!(target: "server_log", "New balance: {}", balance);

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
                                            info!(target: "server_log", "Multiplier: {}", multiplier);

                                            let rewards =
                                                (latest_proof.balance - old_proof.balance) as i64;
                                            let dec_rewards = (rewards as f64)
                                                / 10f64.powf(ORE_TOKEN_DECIMALS as f64);
                                            info!(target: "server_log", "Earned: {} ORE", dec_rewards);

                                            let contributions = {
                                                app_epoch_hashes.read().await.contributions.clone()
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
                                                    best_nonce: u64::from_le_bytes(best_solution.n),
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
                                            info!(target: "server_log", "reset nonce and epoch hashes");

                                            // reset nonce
                                            {
                                                let mut nonce = app_nonce.lock().await;
                                                *nonce = 0;
                                            }
                                            // reset epoch hashes
                                            {
                                                let mut mut_epoch_hashes =
                                                    app_epoch_hashes.write().await;
                                                mut_epoch_hashes.challenge = latest_proof.challenge;
                                                mut_epoch_hashes.best_hash.solution = None;
                                                mut_epoch_hashes.best_hash.difficulty = 0;
                                                mut_epoch_hashes.contributions = HashMap::new();
                                            }
                                            // unset mining pause flag to start new mining mission
                                            info!(target: "server_log", "resume new mining mission");
                                            PAUSED.store(false, Relaxed);

                                            // Mission completed, send signal to tx sender
                                            if let Err(_) = mission_completed_sender.send(0) {
                                                error!(target: "server_log", "The mission completed receiver dropped.");
                                            }

                                            // last one, notify slack and other messaging channels if necessary
                                            if difficulty.ge(&*slack_difficulty)
                                                || difficulty.ge(&*messaging_diff)
                                            {
                                                if messaging_flags.contains(MessagingFlags::SLACK) {
                                                    let _ = slack_message_sender.send(
                                                        RewardsMessage::Rewards(
                                                            difficulty,
                                                            dec_rewards,
                                                            balance,
                                                        ),
                                                    );
                                                }

                                                if messaging_flags.contains(MessagingFlags::DISCORD)
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
                                    // all tx related activities have succeeded, exit tx submit loop
                                    break;
                                } else {
                                    error!(target: "server_log", "Oops! The mission completed sender dropped.");
                                    warn!(target: "server_log", "The laned tx attender task may fail since no mission completion message received.");
                                    break;
                                }
                            }
                        } else {
                            error!(target: "server_log", "Solution is_some but got none on best hash re-check?");
                            tokio::time::sleep(Duration::from_millis(1_000)).await;
                        }
                    }
                    if !success {
                        error!(target: "server_log", "❌ Failed to land tx... either reached {SUBMIT_LIMIT} attempts or ix error or invalid solution.");
                        info!(target: "server_log", "Discarding and refreshing data...");
                        info!(target: "server_log", "refresh proof");
                        if let Ok(refreshed_proof) = get_proof(&rpc_client, wallet_pubkey).await {
                            let mut app_proof = app_proof.lock().await;
                            *app_proof = refreshed_proof;
                            drop(app_proof);
                        }
                        info!(target: "server_log", "reset nonce and epoch hashes");
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
                        info!(target: "server_log", "resume new mining mission");
                        PAUSED.store(false, Relaxed);
                    }
                    tokio::time::sleep(Duration::from_millis(500)).await;
                } else {
                    // solution is None
                    // solution_is_none_counter += 1;
                    if solution_is_none_counter % NO_BEST_SOLUTION_INTERVAL == 0 {
                        info!(target: "server_log", "No best solution yet.");
                    }
                    solution_is_none_counter += 1;
                    tokio::time::sleep(Duration::from_millis(1_000)).await;
                }
            } else {
                // Fall in buffer time window
                debug!(target: "server_log",
                    "Enter buffer time window that spans {} seconds. Standby!",
                    *app_buffer_time
                );
                tokio::time::sleep(Duration::from_millis(1_000)).await;
            }
        } else {
            // cutoff > 0
            // reset none solution counter
            solution_is_none_counter = 0;

            info!(target: "server_log", "Time to cutoff: {}s. Sleep...", cutoff);

            tokio::time::sleep(Duration::from_secs(cutoff as u64)).await;
        };
    }
}
