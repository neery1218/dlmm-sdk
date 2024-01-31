use jito_searcher_client::send_bundle_no_wait;
use std::{env, str::FromStr, sync::Arc, time::Duration};
use std::sync::RwLock;

use clap::{Parser, Subcommand};
use env_logger::TimestampPrecision;
use futures_util::StreamExt;
use jito_protos::{
    convert::versioned_tx_from_packet,
    searcher::{
        mempool_subscription, searcher_service_client::SearcherServiceClient,
        ConnectedLeadersRequest, GetTipAccountsRequest, MempoolSubscription,
        NextScheduledLeaderRequest, PendingTxNotification, ProgramSubscriptionV0,
        SubscribeBundleResultsRequest, WriteLockedAccountSubscriptionV0,
    },
};
use jito_searcher_client::{
    get_searcher_client, send_bundle_with_confirmation, token_authenticator::ClientInterceptor,
};
use log::info;
use solana_address_lookup_table_program::{self, state::AddressLookupTable};
use anchor_client::solana_client::nonblocking::rpc_client::RpcClient;
use solana_program::{hash::Hash, instruction::Instruction};
use solana_sdk::{
    address_lookup_table_account::AddressLookupTableAccount,
    commitment_config::CommitmentConfig,
    pubkey::Pubkey,
    signature::{read_keypair_file, Keypair, Signer},
    system_instruction::transfer,
    sysvar::instructions,
    transaction::{Transaction, VersionedTransaction},
};
use spl_memo::build_memo;
use tokio::time::{sleep, timeout};
use tonic::{codegen::InterceptedService, transport::Channel, Streaming};

pub async fn send_txn_as_bundle(
    rpc_client: &RpcClient,
    block_engine_url: &str,
    payer_keypair: Arc<Keypair>,
    tip_account: Pubkey,
    tip_amount: u64,
    inxs: Vec<Vec<Instruction>>,
) -> Result<(), Box<dyn std::error::Error>> {
    // blockhashes
    let shared_data = Arc::new(RwLock::new(
        rpc_client.get_latest_blockhash().await.unwrap(),
    ));

    let balance = rpc_client
        .get_balance(&payer_keypair.pubkey())
        .await
        .expect("reads balance");
    info!(
        "payer public key: {:?} lamports: {:?}",
        payer_keypair.pubkey(),
        balance
    );

    let writer_data = Arc::clone(&shared_data);
    let url = rpc_client.url().to_string();
    tokio::spawn(async move {
        let rpc_client = RpcClient::new(url);
        loop {
            let blockhash = rpc_client.get_recent_blockhash().await;
            if let Ok((blockhash, _)) = blockhash {
                let mut data = writer_data.write().unwrap();
                *data = blockhash;
            }

            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    });

    let payer_pubkey = &payer_keypair.pubkey();

    let mut client = get_searcher_client(block_engine_url, &payer_keypair)
        .await
        .expect("connects to searcher client");

    let mut bundle_results_subscription = client
        .subscribe_bundle_results(SubscribeBundleResultsRequest {})
        .await
        .expect("subscribe to bundle results")
        .into_inner();

    // wait for jito-solana leader slot
    // let mut is_leader_slot = false;
    // while !is_leader_slot {
    //     let next_leader = client
    //         .get_next_scheduled_leader(NextScheduledLeaderRequest {})
    //         .await
    //         .expect("gets next scheduled leader")
    //         .into_inner();
    //     let num_slots = next_leader.next_leader_slot - next_leader.current_slot;
    //     is_leader_slot = num_slots <= 2;
    //     info!("next jito leader slot in {num_slots} slots");
    //     sleep(Duration::from_millis(500)).await;
    // }

    let mut last_blockhash = None;
    loop {
        // build + sign the transactions
        if let Ok(data) = shared_data.try_read() {
            if last_blockhash != Some(*data) {
                // println!("jito blockhash: {:?}", data);
                last_blockhash = Some(*data);
            }
        }

        if last_blockhash.is_none() {
            continue;
        }

        // doing some bullshit to add tip
        for actual_ixs in &inxs {
            let mut instructions2 = actual_ixs.clone();
            instructions2.push(transfer(
                payer_pubkey,
                &tip_account,
                tip_amount,
            ));

            let txs: Vec<_> = [VersionedTransaction::from(
                Transaction::new_signed_with_payer(
                    instructions2.as_slice(),
                    Some(payer_pubkey),
                    &[payer_keypair.as_ref()],
                    last_blockhash.unwrap(),
                ),
            )]
            .into();

            let _res = send_bundle_no_wait(&txs, &mut client).await;
            // let res = send_bundle_with_confirmation(
            //     &txs,
            //     rpc_client,
            //     &mut client,
            //     &mut bundle_results_subscription,
            // )
            // .await;
            // info!("send_bundle_with_confirmation result: {:?}", res);

            sleep(Duration::from_millis(400)).await;
        }
    }
}

