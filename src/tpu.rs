use {
    solana_client::{
        connection_cache::ConnectionCache,
        nonblocking::tpu_client::TpuClient,
        send_and_confirm_transactions_in_parallel::{
            send_and_confirm_transactions_in_parallel, SendAndConfirmConfig,
        },
        tpu_client::TpuClientConfig,
    },
    solana_rpc_client::nonblocking::rpc_client::RpcClient,
    solana_sdk::{
        instruction::Instruction,
        message::Message,
        signature::{Keypair, Signer},
        // signers::Signers,
    },
    // solana_tpu_client::tpu_client::{Result, TpuSenderError},
    std::sync::Arc,
    tracing::error,
};

pub async fn send_and_confirm(
    rpc_client: &Arc<RpcClient>,
    instructions: &[Instruction],
    // signers: &T,
    signer: &Keypair,
    config: SendAndConfirmConfig,
    // ) -> Result<Vec<Option<TransactionError>>> {
) -> Result<(), Box<dyn std::error::Error>> {
    let blockhash = rpc_client.get_latest_blockhash().await?;
    let websocket_url = rpc_client.url().replace("https", "wss");
    // let config = SendAndConfirmConfig { resign_txs_count: Some(5), with_spinner: true };

    let message = Message::new_with_blockhash(instructions, Some(&signer.pubkey()), &blockhash);
    let messages = &[message];

    // let use_quic = true;
    // let connection_cache = if use_quic {
    //     ConnectionCache::new_quic("connection_cache_ore_ppl_quic", 1)
    // } else {
    //     ConnectionCache::with_udp("connection_cache_ore_ppl_udp", 1)
    // };

    let connection_cache = ConnectionCache::new_quic("connection_cache_ore_ppl_quic", 1);

    let transaction_errors = match connection_cache {
        // ConnectionCache::Udp(cache) => {
        //     TpuClient::new_with_connection_cache(
        //         rpc_client.clone(),
        //         websocket_url.clone().as_str(),
        //         TpuClientConfig::default(),
        //         cache,
        //     )
        //     .await?
        //     .send_and_confirm_messages_with_spinner(messages, &[signer])
        //     .await
        // },
        ConnectionCache::Quic(cache) => {
            let tpu_client = TpuClient::new_with_connection_cache(
                rpc_client.clone(),
                websocket_url.clone().as_str(),
                TpuClientConfig::default(),
                cache,
            )
            .await?;
            send_and_confirm_transactions_in_parallel(
                rpc_client.clone(),
                Some(tpu_client),
                messages,
                &[signer],
                config,
            )
            .await
        },

        _ => return Err(format!("unsupported connection cache").into()),
    }
    .map_err(|err| format!("Data writes to account on-chain failed: {err}"))?
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();

    if !transaction_errors.is_empty() {
        for transaction_error in &transaction_errors {
            error!(target: "server_log", "Transaction Err: {:?}", transaction_error);
        }
        return Err(format!("{} write transactions failed", transaction_errors.len()).into());
    }

    Ok(())
}
