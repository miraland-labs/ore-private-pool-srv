use std::{sync::Arc, time::Duration};

use crate::utils::proof_pubkey;
use base64::{prelude::BASE64_STANDARD, Engine};
use futures::StreamExt;
use ore_api::state::Proof;
use ore_utils::AccountDeserialize;
use solana_account_decoder::UiAccountEncoding;
use solana_client::{nonblocking::pubsub_client::PubsubClient, rpc_config::RpcAccountInfoConfig};
use solana_sdk::{commitment_config::CommitmentConfig, signature::Keypair, signer::Signer};
use tokio::sync::Mutex;
use tracing::{error, info};

pub async fn proof_tracking_processor(
    ws_url: String,
    wallet: Arc<Keypair>,
    proof: Arc<Mutex<Proof>>,
) {
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
