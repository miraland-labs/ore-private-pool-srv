use std::{
    collections::{HashMap, HashSet},
    net::SocketAddr,
    ops::Range,
    sync::{atomic::Ordering::Relaxed, Arc},
    time::Duration,
};

use axum::extract::ws::Message;
use base64::{prelude::BASE64_STANDARD, Engine};
use futures::SinkExt;
use ore_api::state::Proof;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use tokio::sync::{Mutex, RwLock};
use tracing::{error, info};

use crate::{
    message::ServerMessageStartMining,
    utils::{get_cutoff, get_cutoff_with_risk},
    AppState, EpochHashes, PAUSED,
};

pub async fn ready_clients_processor(
    rpc_client: Arc<RpcClient>,
    shared_state: Arc<RwLock<AppState>>,
    app_proof: Arc<Mutex<Proof>>,
    epoch_hashes: Arc<RwLock<EpochHashes>>,
    ready_clients: Arc<Mutex<HashSet<SocketAddr>>>,
    app_nonce: Arc<Mutex<u64>>,
    client_nonce_ranges: Arc<RwLock<HashMap<Pubkey, Range<u64>>>>,
    buffer_time: Arc<u64>,
    risk_time: Arc<u64>,
) {
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

            let cutoff = if (*risk_time).gt(&0) {
                get_cutoff_with_risk(&rpc_client, proof, *buffer_time, *risk_time).await
            } else {
                get_cutoff(&rpc_client, proof, *buffer_time).await
            };
            let mut should_mine = true;

            let cutoff = if cutoff <= 0 {
                let solution = epoch_hashes.read().await.best_hash.solution;
                if solution.is_some() {
                    should_mine = false;
                }
                0
            } else {
                cutoff
            };

            if should_mine {
                tracing::info!(target: "server_log", "Processing {} ready clients.", clients.len());
                let lock = app_proof.lock().await;
                let proof = lock.clone();
                drop(lock);
                let challenge = proof.challenge;
                info!(target: "contribution_log", "Mission to clients with challenge: {}", BASE64_STANDARD.encode(challenge));
                info!(target: "contribution_log", "and cutoff in: {}s", cutoff);
                for client in clients {
                    let nonce_range = {
                        let mut nonce = app_nonce.lock().await;
                        let start = *nonce;
                        // suppose max hashes possible in 60s for a single client
                        *nonce += 4_000_000;
                        drop(nonce);
                        let nonce_end = start + 4_000_000;
                        let end = nonce_end;
                        start..end
                    };

                    let start_mining_message = ServerMessageStartMining::new(
                        challenge,
                        cutoff,
                        nonce_range.start,
                        nonce_range.end,
                    );

                    let client_nonce_ranges = client_nonce_ranges.clone();
                    let shared_state = shared_state.read().await;
                    let sockets = shared_state.sockets.clone();
                    drop(shared_state);
                    if let Some(sender) = sockets.get(&client) {
                        let sender = sender.clone();
                        tokio::spawn(async move {
                            let _ = sender
                                .socket
                                .lock()
                                .await
                                .send(Message::Binary(start_mining_message.to_message_binary()))
                                .await;
                            let _ = client_nonce_ranges
                                .write()
                                .await
                                .insert(sender.pubkey, nonce_range);
                        });
                    } else {
                        error!("Mission cannot be delivered to client {} because the client no longer exists in the sockets map.", client);
                    }
                    // remove ready client from list
                    let _ = ready_clients.lock().await.remove(&client);
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}
