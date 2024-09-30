use std::{
    collections::{HashMap, HashSet},
    net::SocketAddr,
    ops::Range,
    sync::Arc,
};

use axum::extract::ws::Message;
use chrono::Local;
use futures::SinkExt;
use ore_api::state::Proof;
use solana_sdk::pubkey::Pubkey;
use tokio::{
    sync::{mpsc::UnboundedReceiver, Mutex, RwLock},
    time::Instant,
};
use tracing::{error, info, warn};

use crate::{
    AppState, ClientMessage, EpochHashes, InternalMessageContribution, LastPong, MIN_DIFF,
    MIN_HASHPOWER,
};

pub async fn client_message_processor(
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
                        // info!(target: "server_log", "Client {} is ready!", addr.to_string());
                        let mut ready_clients = ready_clients.lock().await;
                        ready_clients.insert(addr);
                    });
                },
                ClientMessage::Mining(addr) => {
                    info!(target: "server_log", "Client {} has started mining!", addr.to_string());
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
                                error!(target: "server_log", "Client nonce range not set!");
                                return;
                            }
                        };
                        drop(reader);

                        let nonce = u64::from_le_bytes(solution.n);

                        if !nonce_range.contains(&nonce) {
                            error!(target: "server_log", "❌ Client submitted nonce out of assigned range");
                            return;
                        }

                        let reader = app_state.read().await;
                        let miner_id;
                        if let Some(app_client_socket) = reader.sockets.get(&addr) {
                            miner_id = app_client_socket.miner_id;
                        } else {
                            error!(target: "server_log", "Failed to get client socket for addr: {}", addr);
                            return;
                        }
                        drop(reader);

                        let lock = proof.lock().await;
                        let challenge = lock.challenge;
                        drop(lock);
                        if solution.is_valid(&challenge) {
                            let diff = solution.to_hash().difficulty();
                            info!(target: "server_log",
                                "{} found diff: {} at {}",
                                // pubkey_str,
                                short_pbukey_str,
                                diff,
                                Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
                            );
                            info!(target: "contribution_log",
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
                                // if hashpower > 81_920 {
                                //     hashpower = 81_920;
                                // }
                                if hashpower > 655_360 {
                                    hashpower = 655_360;
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
                                                // info!(target: "server_log", "New best diff: {}", diff);
                                                info!(target: "contribution_log", "New best diff: {}", diff);
                                                epoch_hashes.best_hash.difficulty = diff;
                                                epoch_hashes.best_hash.solution = Some(solution);
                                            }
                                            drop(epoch_hashes);
                                        } else {
                                            info!(target: "server_log", "Miner submitted lower diff than a previous contribution, discarding lower diff");
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
                                            // info!(target: "server_log", "New best diff: {}", diff);
                                            info!(target: "contribution_log", "New best diff: {}", diff);
                                            epoch_hashes.best_hash.difficulty = diff;
                                            epoch_hashes.best_hash.solution = Some(solution);
                                        }
                                        drop(epoch_hashes);
                                    }
                                }
                                // tokio::time::sleep(Duration::from_millis(100)).await;
                            } else {
                                warn!(target: "server_log", "Diff too low, skipping");
                            }
                        } else {
                            error!(target: "server_log",
                                "{} returned an invalid solution for latest challenge!",
                                // pubkey
                                short_pbukey_str
                            );

                            let reader = app_state.read().await;
                            if let Some(app_client_socket) = reader.sockets.get(&addr) {
                                let _ = app_client_socket.socket.lock().await.send(Message::Text("Invalid solution. If this keeps happening, please contact support.".to_string())).await;
                            } else {
                                error!(target: "server_log", "Failed to get client socket for addr: {}", addr);
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
