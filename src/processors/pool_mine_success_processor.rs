use axum::extract::ws::Message;
use futures::SinkExt;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{native_token::LAMPORTS_PER_SOL, signer::Signer};
use std::{ops::Div, str::FromStr, sync::Arc, time::Duration};
use tokio::sync::{mpsc::UnboundedReceiver, RwLock};
use tracing::{error, info};

use crate::{
    database::{Database, PoweredByDbms},
    message::ServerMessagePoolSubmissionResult,
    utils::ORE_TOKEN_DECIMALS,
    AppState, ClientVersion, InsertContribution, InsertEarning, InternalMessageContribution,
    MessageInternalMineSuccess, MineConfig, UpdateReward, WalletExtension, POWERED_BY_DBMS,
};

pub async fn pool_mine_success_processor(
    app_rpc_client: Arc<RpcClient>,
    app_mine_config: Arc<MineConfig>,
    app_shared_state: Arc<RwLock<AppState>>,
    app_database: Arc<Database>,
    app_wallet: Arc<WalletExtension>,
    mut mine_success_receiver: UnboundedReceiver<MessageInternalMineSuccess>,
) {
    let database = app_database;
    let mine_config = app_mine_config;
    loop {
        let mut sol_balance_checking = 0_u64;
        while let Some(msg) = mine_success_receiver.recv().await {
            let decimals = 10f64.powf(ORE_TOKEN_DECIMALS as f64);
            let top_stake = if let Some(config) = msg.ore_config {
                (config.top_balance as f64).div(decimals)
            } else {
                1.0f64
            };

            let powered_by_dbms = POWERED_BY_DBMS.get_or_init(|| {
                let key = "POWERED_BY_DBMS";
                match std::env::var(key) {
                    Ok(val) => PoweredByDbms::from_str(&val)
                        .expect("POWERED_BY_DBMS must be set correctly."),
                    Err(_) => PoweredByDbms::Unavailable,
                }
            });
            if powered_by_dbms == &PoweredByDbms::Sqlite {
                let mut i_earnings = Vec::new();
                let mut i_rewards = Vec::new();
                let mut i_contributions = Vec::new();

                for (miner_pubkey, msg_contribution) in msg.contributions.iter() {
                    let hashpower_percent = (msg_contribution.hashpower as u128)
                        .saturating_mul(1_000_000)
                        .saturating_div(msg.total_hashpower as u128);

                    // TODO: handle overflow/underflow and float imprecision issues
                    // let decimals = 10f64.powf(ORE_TOKEN_DECIMALS as f64);
                    let earned_rewards = hashpower_percent
                        .saturating_mul(msg.rewards as u128)
                        .saturating_div(1_000_000) as i64;

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

                    // let top_stake = if let Some(config) = msg.ore_config {
                    //     (config.top_balance as f64).div(decimals)
                    // } else {
                    //     1.0f64
                    // };

                    let shared_state = app_shared_state.read().await;
                    let len = shared_state.sockets.len();
                    let socks = shared_state.sockets.clone();
                    drop(shared_state);

                    for (_addr, client_connection) in socks.iter() {
                        if client_connection.pubkey.eq(&miner_pubkey) {
                            let socket_sender = client_connection.socket.clone();
                            match client_connection.client_version {
                                ClientVersion::V0 => {
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
                                    tokio::spawn(async move {
                                        if let Ok(_) = socket_sender
                                            .lock()
                                            .await
                                            .send(Message::Text(message))
                                            .await
                                        {
                                        } else {
                                            error!("Failed to send client text");
                                        }
                                    });
                                },
                                ClientVersion::V1 => {
                                    let server_message = ServerMessagePoolSubmissionResult::new(
                                        msg.difficulty,
                                        msg.total_balance,
                                        pool_rewards_dec,
                                        top_stake,
                                        msg.multiplier,
                                        len as u32,
                                        msg.challenge,
                                        msg.best_nonce,
                                        msg_contribution.supplied_diff as u32,
                                        earned_rewards_dec,
                                        percentage,
                                    );
                                    tokio::spawn(async move {
                                        if let Ok(_) = socket_sender
                                            .lock()
                                            .await
                                            .send(Message::Binary(
                                                server_message.to_message_binary(),
                                            ))
                                            .await
                                        {
                                        } else {
                                            error!("Failed to send client pool submission result binary message");
                                        }
                                    });
                                },
                            }
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
                    .update_pool_rewards(app_wallet.miner_wallet.pubkey().to_string(), msg.rewards)
                    .await
                {
                    error!("Failed to update pool rewards! Retrying...");
                    tokio::time::sleep(Duration::from_millis(1000)).await;
                }

                tokio::time::sleep(Duration::from_millis(200)).await;
                let contribution_id;
                loop {
                    if let Ok(cid) = database.get_contribution_id_with_nonce(msg.best_nonce).await {
                        contribution_id = cid;
                        break;
                    } else {
                        error!("Failed to get contribution id with nonce! Retrying...");
                        tokio::time::sleep(Duration::from_millis(1000)).await;
                    }
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
                if let Err(_) = database
                    .update_challenge_rewards(msg.challenge.to_vec(), contribution_id, msg.rewards)
                    .await
                {
                    error!("Failed to update challenge rewards! Skipping! Devs check!");
                    let err_str = format!("Challenge UPDATE FAILED - Challenge: {:?}\nContribution ID: {}\nRewards: {}\n", msg.challenge.to_vec(), contribution_id, msg.rewards);
                    error!(err_str);
                }
            } else {
                // let decimals = 10f64.powf(ORE_TOKEN_DECIMALS as f64);
                let pool_rewards_dec = (msg.rewards as f64).div(decimals);
                let shared_state = app_shared_state.read().await;
                let len = shared_state.sockets.len();
                let socks = shared_state.sockets.clone();
                drop(shared_state);

                for (_addr, client_connection) in socks.iter() {
                    let socket_sender = client_connection.socket.clone();
                    let pubkey = client_connection.pubkey;

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

                        match client_connection.client_version {
                            ClientVersion::V0 => {
                                let message = format!(
                                        "Pool Submitted Difficulty: {}\nPool Earned:  {:.11} ORE\nPool Balance: {:.11} ORE\nTop Stake:    {:.11} ORE\nPool Multiplier: {:.2}x\n----------------------\nActive Miners: {}\n----------------------\nMiner Submitted Difficulty: {}\nMiner Earned: {:.11} ORE\n{:.2}% of total pool reward",
                                        msg.difficulty,
                                        pool_rewards_dec,
                                        msg.total_balance,
                                        top_stake,
                                        msg.multiplier,
                                        len,
                                        supplied_diff,
                                        earned_rewards_dec,
                                        percentage
                                    );
                                tokio::spawn(async move {
                                    if let Ok(_) = socket_sender
                                        .lock()
                                        .await
                                        .send(Message::Text(message))
                                        .await
                                    {
                                    } else {
                                        error!("Failed to send client text");
                                    }
                                });
                            },
                            ClientVersion::V1 => {
                                let server_message = ServerMessagePoolSubmissionResult::new(
                                    msg.difficulty,
                                    msg.total_balance,
                                    pool_rewards_dec,
                                    top_stake,
                                    msg.multiplier,
                                    len as u32,
                                    msg.challenge,
                                    msg.best_nonce,
                                    *supplied_diff as u32,
                                    earned_rewards_dec,
                                    percentage,
                                );
                                tokio::spawn(async move {
                                    if let Ok(_) = socket_sender
                                        .lock()
                                        .await
                                        .send(Message::Binary(server_message.to_message_binary()))
                                        .await
                                    {
                                    } else {
                                        error!("Failed to send client pool submission result binary message");
                                    }
                                });
                            },
                        }
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
}
