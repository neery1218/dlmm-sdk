use std::{env, str::FromStr, sync::Arc, time::Duration};

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
use solana_client::nonblocking::rpc_client::{self, RpcClient};
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

async fn send_txn_as_bundle(
    rpc_client: RpcClient,
    block_engine_url: &str,
    payer_keypair: Arc<Keypair>,
    tip_account: Pubkey,
    tip_amount: u64,
    inxs: &Vec<Instruction>,
    blockhash: Hash,
) -> Result<(), Box<dyn std::error::Error>> {
    let balance = rpc_client
        .get_balance(&payer_keypair.pubkey())
        .await
        .expect("reads balance");
    let payer_pubkey = &payer_keypair.pubkey();

    info!(
        "payer public key: {:?} lamports: {:?}",
        payer_keypair.pubkey(),
        balance
    );
    let mut client = get_searcher_client(block_engine_url, &payer_keypair)
        .await
        .expect("connects to searcher client");

    let mut bundle_results_subscription = client
        .subscribe_bundle_results(SubscribeBundleResultsRequest {})
        .await
        .expect("subscribe to bundle results")
        .into_inner();

    // wait for jito-solana leader slot
    let mut is_leader_slot = false;
    while !is_leader_slot {
        let next_leader = client
            .get_next_scheduled_leader(NextScheduledLeaderRequest {})
            .await
            .expect("gets next scheduled leader")
            .into_inner();
        let num_slots = next_leader.next_leader_slot - next_leader.current_slot;
        is_leader_slot = num_slots <= 2;
        info!("next jito leader slot in {num_slots} slots");
        sleep(Duration::from_millis(500)).await;
    }

    // build + sign the transactions
    let blockhash = rpc_client
        .get_latest_blockhash()
        .await
        .expect("get blockhash");

    // doing some bullshit to add tip
    let mut instructions2 = inxs.clone();
    instructions2.append(&mut Vec::from(&[transfer(
        payer_pubkey,
        &tip_account,
        tip_amount,
    )]));

    let txs: Vec<_> = [VersionedTransaction::from(
        Transaction::new_signed_with_payer(
            instructions2.as_slice(),
            Some(payer_pubkey),
            &[payer_keypair.as_ref()],
            blockhash,
        ),
    )]
    .into();

    send_bundle_with_confirmation(
        &txs,
        &rpc_client,
        &mut client,
        &mut bundle_results_subscription,
    )
    .await
    .expect("Sending bundle failed");
    Ok(())
}

