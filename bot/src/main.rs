use anchor_client::solana_client::nonblocking::rpc_client::RpcClient;
use anchor_client::solana_client::rpc_config::RpcSendTransactionConfig;
use anchor_client::solana_sdk::transaction::Transaction;
use anchor_client::Client;
use solana_client::nonblocking::tpu_client::TpuClient;
use solana_client::tpu_client::TpuClientConfig;
use spl_memo::build_memo;
use std::ops::Deref;
use std::result::Result::Ok;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::RwLock;
use std::time::Instant;

use anchor_client::solana_sdk::signer::keypair::read_keypair_file;
use lb_clmm::constants::BASIS_POINT_MAX;
use lb_clmm::math::u128x128_math::Rounding;
use rust_decimal::MathematicalOps;
use rust_decimal::{
    prelude::{FromPrimitive, ToPrimitive},
    Decimal,
};

use anchor_client::solana_sdk::compute_budget::ComputeBudgetInstruction;
use anchor_client::solana_sdk::instruction::Instruction;
use anchor_client::{solana_sdk::pubkey::Pubkey, solana_sdk::signer::Signer, Program};
use anchor_lang::solana_program::instruction::AccountMeta;

use anchor_spl::associated_token::get_associated_token_address;

use anyhow::*;
use lb_clmm::accounts;
use lb_clmm::constants::MAX_BIN_PER_ARRAY;
use lb_clmm::instruction;

use lb_clmm::math::u64x64_math::{from_decimal, to_decimal};
use lb_clmm::state::bin::BinArray;
use lb_clmm::state::lb_pair::LbPair;
use lb_clmm::utils::pda::*;

#[derive(Debug)]
pub struct SwapParameters {
    pub lb_pair: Pubkey,
    pub amount_in: u64,
    pub swap_for_y: bool,
    pub bin_array_idx: i32,
    pub min_amount_out: u64,
}

pub async fn swap<C: Deref<Target = impl Signer> + Clone>(
    params: SwapParameters,
    program: &Program<C>,
    lb_pair_state: &LbPair,
) -> Result<Vec<Instruction>> {
    let SwapParameters {
        amount_in,
        lb_pair,
        swap_for_y,
        bin_array_idx,
        min_amount_out,
    } = params;

    let (bin_array_0, _bump) = derive_bin_array_pda(lb_pair, bin_array_idx as i64);
    let (user_token_in, user_token_out, bin_array_1, bin_array_2) = if swap_for_y {
        (
            get_associated_token_address(&program.payer(), &lb_pair_state.token_x_mint),
            get_associated_token_address(&program.payer(), &lb_pair_state.token_y_mint),
            derive_bin_array_pda(lb_pair, (bin_array_idx - 1) as i64).0,
            derive_bin_array_pda(lb_pair, (bin_array_idx - 2) as i64).0,
        )
    } else {
        (
            get_associated_token_address(&program.payer(), &lb_pair_state.token_y_mint),
            get_associated_token_address(&program.payer(), &lb_pair_state.token_x_mint),
            derive_bin_array_pda(lb_pair, (bin_array_idx + 1) as i64).0,
            derive_bin_array_pda(lb_pair, (bin_array_idx + 2) as i64).0,
        )
    };

    let (bin_array_bitmap_extension, _bump) = derive_bin_array_bitmap_extension(lb_pair);
    let bin_array_bitmap_extension = if program
        .rpc()
        .get_account(&bin_array_bitmap_extension)
        .is_err()
    {
        None
    } else {
        Some(bin_array_bitmap_extension)
    };

    let (event_authority, _bump) =
        Pubkey::find_program_address(&[b"__event_authority"], &lb_clmm::ID);

    let accounts = accounts::Swap {
        lb_pair,
        bin_array_bitmap_extension,
        reserve_x: lb_pair_state.reserve_x,
        reserve_y: lb_pair_state.reserve_y,
        token_x_mint: lb_pair_state.token_x_mint,
        token_y_mint: lb_pair_state.token_y_mint,
        token_x_program: anchor_spl::token::ID,
        token_y_program: anchor_spl::token::ID,
        user: program.payer(),
        user_token_in,
        user_token_out,
        oracle: lb_pair_state.oracle,
        host_fee_in: Some(lb_clmm::ID),
        event_authority,
        program: lb_clmm::ID,
    };

    let ix = instruction::Swap {
        amount_in,
        min_amount_out,
    };

    let remaining_accounts = vec![
        AccountMeta {
            is_signer: false,
            is_writable: true,
            pubkey: bin_array_0,
        },
        AccountMeta {
            is_signer: false,
            is_writable: true,
            pubkey: bin_array_1,
        },
        AccountMeta {
            is_signer: false,
            is_writable: true,
            pubkey: bin_array_2,
        },
    ];

    let compute_budget_ix = ComputeBudgetInstruction::set_compute_unit_limit(1_400_000);

    let request_builder = program.request();
    Ok(request_builder
        .instruction(compute_budget_ix)
        .accounts(accounts)
        .accounts(remaining_accounts)
        .args(ix)
        .instructions()?)
}

#[tokio::main]
async fn main() {
    // params
    let amount_in = 1_000;
    let target_price = 0.4;
    let swap_for_y = true; // true if dumping X, false if buying X

    // JUP-USDC. x is JUP, y is USDC
    let pool = Pubkey::from_str("GhZtugCqUskpDPiuB5zJPxabxpZZKuPZmAQtfSDB3jpZ").unwrap();
    let decimals_x = 6;
    let decimals_y = 6;

    // SOL-USDC. x is SOL, y is USDC
    // let pool = Pubkey::from_str("FoSDw2L5DmTuQTFe55gWPDXf88euaxAEKFre74CnvQbX").unwrap();
    // let decimals_x = 9;
    // let decimals_y = 6;

    // pyth-usdc
    let pool = Pubkey::from_str("7ER8z7q6RLE3EXL3m9SaH68Lei6ba8yv9APC1iq8duJG").unwrap();
    let decimals_x = 6;
    let decimals_y = 6;

    let payer = read_keypair_file("/home/ubuntu/.config/solana/id.json")
        .expect("Wallet keypair file not found");
    let client = Client::new(anchor_client::Cluster::Mainnet, &payer);

    let program = client.program(lb_clmm::ID).unwrap();

    let lb_pair_state: LbPair = program.account(pool).await.unwrap();

    // FIXME: need to find an actual price here
    let expected_out_amount = if swap_for_y {
        // sol is x and usdc is y and we are swapping for y, we want to sell sol at a price of 100 usdc
        // or higher
        // total dollars expected = 100 * amount_in / 10.pow(decimals_x)
        // total usdc expected = total dollars expected * 10.pow(decimals_y)
        target_price * amount_in as f64 / 10_f64.powi(decimals_x as i32 - decimals_y as i32)
    } else {
        // sol is x and usdc is y and we are swapping for y, we want to buy sol at a price of 100
        // or lower
        // dollar value of input = amount_in / 10.pow(decimals_y);
        // expected total sol per $1 = (dollar value of input / target_price) * 10.pow(decimals_x)
        amount_in as f64 / target_price * 10_f64.powi(decimals_x as i32 - decimals_y as i32)
    };
    println!("expected_out_amount: {:?}", expected_out_amount);

    let price_per_lamport =
        price_per_token_to_per_lamport(target_price, decimals_x, decimals_y).unwrap();

    println!("lb pair state: {:#?}", lb_pair_state);
    println!("bin step: {:?}", lb_pair_state.bin_step);
    let bin_id =
        get_id_from_price(lb_pair_state.bin_step, &price_per_lamport, Rounding::Down).unwrap();
    println!("bin_id: {:?}", bin_id);

    let mut bin_array_idx = BinArray::bin_id_to_bin_array_index(bin_id).unwrap();
    println!("bin_array_idx: {:?}", bin_array_idx);
    println!("active bin id: {:?}", lb_pair_state.active_id);
    println!(
        "active bin array id: {:?}",
        BinArray::bin_id_to_bin_array_index(lb_pair_state.active_id)
    );
    let active_bin_array_idx =
        BinArray::bin_id_to_bin_array_index(lb_pair_state.active_id).unwrap();
    if swap_for_y && active_bin_array_idx > bin_array_idx
        || !swap_for_y && active_bin_array_idx < bin_array_idx
    {
        bin_array_idx = active_bin_array_idx;
    }

    let offsets = if swap_for_y { [0, 1, 2] } else { [0, -1, -2] };

    let mut nested_swap_ixes = vec![];
    for offset in offsets {
        let swap_ixes = swap(
            SwapParameters {
                lb_pair: pool,
                amount_in,
                swap_for_y,
                bin_array_idx: bin_array_idx + offset,
                min_amount_out: expected_out_amount as u64,
            },
            &program,
            &lb_pair_state,
        )
        .await
        .unwrap();
        nested_swap_ixes.push(swap_ixes);
    }

    println!("spamming...");
    let h = tokio::spawn(async move {
        executor(nested_swap_ixes).await;
    });
    h.await.unwrap();
}

/// Calculate the bin id based on price. If the bin id is in between 2 bins, it will round up.
pub fn get_id_from_price(bin_step: u16, price: &Decimal, rounding: Rounding) -> Option<i32> {
    let bps = Decimal::from_u16(bin_step)?.checked_div(Decimal::from_i32(BASIS_POINT_MAX)?)?;
    let base = Decimal::ONE.checked_add(bps)?;

    let id = match rounding {
        Rounding::Down => price.log10().checked_div(base.log10())?.floor(),
        Rounding::Up => price.log10().checked_div(base.log10())?.ceil(),
    };

    id.to_i32()
}

/// Convert Q64xQ64 price to human readable decimal. This is price per lamport.
pub fn q64x64_price_to_decimal(q64x64_price: u128) -> Option<Decimal> {
    let q_price = Decimal::from_u128(q64x64_price)?;
    let scale_off = Decimal::TWO.powu(lb_clmm::math::u64x64_math::SCALE_OFFSET.into());
    q_price.checked_div(scale_off)
}

/// price_per_lamport = price_per_token * 10 ** quote_token_decimal / 10 ** base_token_decimal
pub fn price_per_token_to_per_lamport(
    price_per_token: f64,
    base_token_decimal: u8,
    quote_token_decimal: u8,
) -> Option<Decimal> {
    let price_per_token = Decimal::from_f64(price_per_token)?;
    Some(
        price_per_token
            .checked_mul(Decimal::TEN.powu(quote_token_decimal.into()))?
            .checked_div(Decimal::TEN.powu(base_token_decimal.into()))?,
    )
}

/// price_per_token = price_per_lamport * 10 ** base_token_decimal / 10 ** quote_token_decimal, Solve for price_per_lamport
pub fn price_per_lamport_to_price_per_token(
    price_per_lamport: f64,
    base_token_decimal: u8,
    quote_token_decimal: u8,
) -> Option<Decimal> {
    let one_ui_base_token_amount = Decimal::TEN.powu(base_token_decimal.into());
    let one_ui_quote_token_amount = Decimal::TEN.powu(quote_token_decimal.into());
    let price_per_lamport = Decimal::from_f64(price_per_lamport)?;

    Some(
        one_ui_base_token_amount
            .checked_mul(price_per_lamport)?
            .checked_div(one_ui_quote_token_amount)?,
    )
}

pub async fn executor(swap_ixes: Vec<Vec<Instruction>>) {
    let rpc_url = "https://solend.rpcpool.com/a3e03ba77d5e870c8c694b19d61c";
    let ws_url = "ws://solend.rpcpool.com/a3e03ba77d5e870c8c694b19d61c:8900";

    let rpc_client = RpcClient::new(rpc_url.to_string());
    let shared_data = Arc::new(RwLock::new(
        rpc_client.get_recent_blockhash().await.unwrap().0,
    ));
    let tpu_client = TpuClient::new(
        "tpu_client",
        Arc::new(rpc_client),
        ws_url,
        TpuClientConfig::default(),
    )
    .await
    .unwrap();

    let writer_data = Arc::clone(&shared_data);
    let h1 = tokio::spawn(async move {
        let rpc_client = RpcClient::new(rpc_url.to_string());
        loop {
            let blockhash = rpc_client.get_recent_blockhash().await;
            if let Ok((blockhash, _)) = blockhash {
                let mut data = writer_data.write().unwrap();
                *data = blockhash;
            }

            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    });

    // create a tokio task that reads from shared_data and prints out the value
    let rpc_urls = [
        "https://rpc-proxy-miami.sayori.link/42Mv0wgetfWgq6YRXbGbkUeRp49qDuFS",
        "https://solend.rpcpool.com/a3e03ba77d5e870c8c694b19d61c",
        "http://mainnet.rpc.jito.wtf/",
        "http://frankfurt.mainnet.rpc.jito.wtf/",
    ];

    let mut handles = vec![];
    for rpc_url in rpc_urls {
        let swap_ixes = swap_ixes.clone();
        let shared_data = Arc::clone(&shared_data);

        let h = tokio::spawn(async move {
            let mut last_blockhash = None;
            let rpc_client = RpcClient::new(rpc_url.to_string());

            let payer = read_keypair_file(&*shellexpand::tilde("~/.config/solana/id.json"))
                .expect("Example requires a keypair file");

            // let now = Instant::now();
            let mut i = 0;
            loop {
                if let Ok(data) = shared_data.try_read() {
                    if last_blockhash != Some(*data) {
                        // println!("blockhash: {:?}", data);
                        last_blockhash = Some(*data);
                    }
                }

                for swap_ixes in &swap_ixes {
                    if let Some(blockhash) = last_blockhash {
                        let mut ixes = vec![ComputeBudgetInstruction::set_compute_unit_price(1)];
                        ixes.extend(swap_ixes.clone());
                        ixes.push(build_memo(format!("{}", i).as_bytes(), &[]));

                        let tx = Transaction::new_signed_with_payer(
                            &ixes,
                            Some(&payer.pubkey()),
                            &[&payer],
                            blockhash,
                        );
                        // let sig = rpc_client.send_and_confirm_transaction(&tx).await.unwrap();
                        let sig = rpc_client
                            .send_transaction_with_config(
                                &tx,
                                RpcSendTransactionConfig {
                                    skip_preflight: true,
                                    ..RpcSendTransactionConfig::default()
                                },
                            )
                            .await;
                        // println!("sig: {:?}", sig);
                    }
                }

                i += 1;
                if i % 100 == 0 {
                    println!("{} transactions sent to {}", i, rpc_url);
                }
            }
        });
        handles.push(h);
    }

    for h in handles {
        h.await.unwrap();
    }
}
