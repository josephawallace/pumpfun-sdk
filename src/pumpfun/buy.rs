use anyhow::anyhow;
use solana_sdk::{
    compute_budget::ComputeBudgetInstruction, instruction::Instruction, message::{v0, VersionedMessage}, native_token::sol_to_lamports, pubkey::Pubkey, signature::Keypair, signer::Signer, system_instruction, transaction::{Transaction, VersionedTransaction}
};
use solana_hash::Hash;
use spl_associated_token_account::instruction::create_associated_token_account_idempotent;
use tokio::task::JoinHandle;
use std::{str::FromStr, time::Instant, sync::Arc};
use solana_rpc_client_api::config::RpcSendTransactionConfig;
use solana_sdk::signature::Signature;
use crate::{common::{PriorityFee, SolanaRpcClient}, constants::{self, global_constants::FEE_RECIPIENT}, instruction, swqos::FeeClient};

const MAX_LOADED_ACCOUNTS_DATA_SIZE_LIMIT: u32 = 250000;

use super::common::{calculate_with_slippage_buy, get_bonding_curve_pda, get_creator_vault_pda};

pub async fn buy(
    rpc: Arc<SolanaRpcClient>,
    payer: Arc<Keypair>,
    mint: Pubkey,
    creator: Pubkey,
    buy_token_amount: u64,
    max_amount_sol: u64,
    slippage_basis_points: Option<u64>,
    priority_fee: PriorityFee,
    recent_blockhash: Hash,
) -> Result<Signature, anyhow::Error> {
    let transaction = build_buy_transaction(
        payer.clone(),
        mint.clone(),
        creator.clone(),
        buy_token_amount,
        max_amount_sol,
        slippage_basis_points,
        priority_fee.clone(),
        recent_blockhash,
    ).await?;

    let mut rpc_send_tx_config = RpcSendTransactionConfig::default();
    rpc_send_tx_config.skip_preflight = true;

    let signature = rpc.send_transaction_with_config(&transaction, rpc_send_tx_config).await?;
    Ok(signature)
}

/// Buy tokens using Jito
pub async fn buy_with_tip(
    fee_clients: Vec<Arc<FeeClient>>,
    payer: Arc<Keypair>,
    mint: Pubkey,
    creator: Pubkey,
    buy_token_amount: u64,
    max_amount_sol: u64,
    slippage_basis_points: Option<u64>,
    priority_fee: PriorityFee,
    recent_blockhash: Hash,
) -> Result<(), anyhow::Error> {
    let start_time = Instant::now();

    let mint = Arc::new(mint.clone());
    let creator = Arc::new(creator.clone());
    let instructions = build_buy_instructions(
        payer.clone(),
        mint.clone(),
        creator.clone(),
        buy_token_amount,
        max_amount_sol,
        slippage_basis_points
    ).await?;

    let mut transactions = vec![];
    for fee_client in fee_clients.clone() {
        let payer = payer.clone();
        let priority_fee = priority_fee.clone();
        let tip_account = fee_client.get_tip_account().await.map_err(|e| anyhow!(e.to_string()))?;
        let tip_account = Arc::new(Pubkey::from_str(&tip_account).map_err(|e| anyhow!(e))?);

        let transaction = build_buy_transaction_with_tip(tip_account, payer, priority_fee, instructions.clone(), recent_blockhash).await?;
        transactions.push(transaction);
    }

    let mut handles: Vec<JoinHandle<Result<(), anyhow::Error>>> = vec![];
    for i in 0..fee_clients.len() {
        let fee_client = fee_clients[i].clone();
        let transactions = transactions.clone();
        let start_time = start_time.clone();
        let transaction = transactions[i].clone();
        let handle = tokio::spawn(async move {
           fee_client.send_transaction(&transaction).await?;
            println!("index: {}, Total Jito buy operation time: {:?}ms", i, start_time.elapsed().as_millis());
            Ok::<(), anyhow::Error>(())
        });

        handles.push(handle);        
    }

    for handle in handles {
        match handle.await {
            Ok(Ok(_)) => (),
            Ok(Err(e)) => println!("Error in task: {}", e),
            Err(e) => println!("Task join error: {}", e),
        }
    }

    Ok(())
}

pub async fn build_buy_transaction(
    payer: Arc<Keypair>,
    mint: Pubkey,
    creator: Pubkey,
    buy_token_amount: u64,
    max_amount_sol: u64,
    slippage_basis_points: Option<u64>,
    priority_fee: PriorityFee,
    recent_blockhash: Hash,
) -> Result<Transaction, anyhow::Error> {
    let mut instructions = vec![
        ComputeBudgetInstruction::set_loaded_accounts_data_size_limit(MAX_LOADED_ACCOUNTS_DATA_SIZE_LIMIT),
        ComputeBudgetInstruction::set_compute_unit_price(priority_fee.unit_price),
        ComputeBudgetInstruction::set_compute_unit_limit(priority_fee.unit_limit),
    ];

    let build_instructions = build_buy_instructions(
        payer.clone(),
        Arc::new(mint),
        Arc::new(creator),
        buy_token_amount,
        max_amount_sol,
        slippage_basis_points
    ).await?;
    instructions.extend(build_instructions);

    let transaction = Transaction::new_signed_with_payer(
        &instructions,
        Some(&payer.pubkey()),
        &[payer],
        recent_blockhash,
    );

    Ok(transaction)
}

pub async fn build_buy_transaction_with_tip(
    tip_account: Arc<Pubkey>,
    payer: Arc<Keypair>,
    priority_fee: PriorityFee,  
    build_instructions: Vec<Instruction>,
    blockhash: Hash,
) -> Result<VersionedTransaction, anyhow::Error> {
    let mut instructions = vec![
        ComputeBudgetInstruction::set_loaded_accounts_data_size_limit(MAX_LOADED_ACCOUNTS_DATA_SIZE_LIMIT),
        ComputeBudgetInstruction::set_compute_unit_price(priority_fee.unit_price),
        ComputeBudgetInstruction::set_compute_unit_limit(priority_fee.unit_limit),
        system_instruction::transfer(
            &payer.pubkey(),
            &tip_account,
            sol_to_lamports(priority_fee.buy_tip_fee),
        ),
    ];

    instructions.extend(build_instructions);

    let v0_message: v0::Message =
        v0::Message::try_compile(&payer.pubkey(), &instructions, &[], blockhash)?;
    let versioned_message: VersionedMessage = VersionedMessage::V0(v0_message);
    let transaction = VersionedTransaction::try_new(versioned_message, &[&payer])?;

    Ok(transaction)
}

pub async fn build_buy_instructions(
    payer: Arc<Keypair>,
    mint: Arc<Pubkey>,
    creator: Arc<Pubkey>,
    buy_token_amount: u64,
    max_amount_sol: u64,
    slippage_basis_points: Option<u64>,
) -> Result<Vec<Instruction>, anyhow::Error> {
    if max_amount_sol == 0 {
        return Err(anyhow!("Amount cannot be zero"));
    }

    let bonding_curve_pda = get_bonding_curve_pda(&mint).unwrap();
    let creator_vault_pda = get_creator_vault_pda(&creator).unwrap();
    let max_amount_sol = calculate_with_slippage_buy(max_amount_sol, slippage_basis_points.unwrap_or(100));

    let mut instructions = vec![];
    instructions.push(create_associated_token_account_idempotent(
        &payer.pubkey(),
        &payer.pubkey(),
        &mint,
        &constants::accounts::TOKEN_PROGRAM,
    ));

    instructions.push(instruction::buy(
        payer.as_ref(),
        &mint,
        &bonding_curve_pda,
        &creator_vault_pda,
        &FEE_RECIPIENT,
        instruction::Buy {
            _amount: buy_token_amount,
            _max_sol_cost: max_amount_sol,
        },
    ));

    Ok(instructions)
}