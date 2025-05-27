use anyhow::anyhow;
use solana_sdk::{
    compute_budget::ComputeBudgetInstruction, instruction::Instruction, message::{v0, VersionedMessage}, native_token::sol_to_lamports, pubkey::Pubkey, signature::Keypair, signer::Signer, system_instruction, transaction::{Transaction, VersionedTransaction}
};
use solana_hash::Hash;
use spl_associated_token_account::get_associated_token_address;
use spl_token::instruction::close_account;
use tokio::task::JoinHandle;

use std::{str::FromStr, time::Instant, sync::Arc};
use solana_rpc_client_api::config::RpcSendTransactionConfig;
use solana_sdk::signature::Signature;
use crate::{common::{PriorityFee, SolanaRpcClient}, instruction, swqos::FeeClient};
use crate::constants::global_constants::FEE_RECIPIENT;
use super::common::{get_bonding_curve_pda, get_creator_vault_pda};

async fn get_token_balance(rpc: &SolanaRpcClient, payer: &Keypair, mint: &Pubkey) -> Result<(u64, Pubkey), anyhow::Error> {
    let ata = get_associated_token_address(&payer.pubkey(), mint);
    let balance = rpc.get_token_account_balance(&ata).await?;
    let balance_u64 = balance.amount.parse::<u64>()
        .map_err(|_| anyhow!("Failed to parse token balance"))?;
    
    if balance_u64 == 0 {
        return Err(anyhow!("Balance is 0"));
    }

    Ok((balance_u64, ata))
}

pub async fn sell(
    rpc: Arc<SolanaRpcClient>,
    payer: Arc<Keypair>,
    mint: Pubkey,
    bonding_curve_creator: Pubkey,
    amount_token: Option<u64>,
    priority_fee: PriorityFee,
    recent_blockhash: Hash,
) -> Result<Signature, anyhow::Error> {
    let instructions = build_sell_instructions(payer.clone(), mint.clone(), bonding_curve_creator, amount_token).await?;
    let transaction = build_sell_transaction(payer.clone(), priority_fee, instructions, recent_blockhash).await?;
    
    let mut rpc_send_tx_config = RpcSendTransactionConfig::default();
    rpc_send_tx_config.skip_preflight = true;

    let signature = rpc.send_transaction_with_config(&transaction, rpc_send_tx_config).await?;

    Ok(signature)
}

/// Sell tokens by percentage
pub async fn sell_by_percent(
    rpc: Arc<SolanaRpcClient>,
    payer: Arc<Keypair>,
    mint: Pubkey,
    bonding_curve_creator: Pubkey,
    percent: u64,
    priority_fee: PriorityFee,
    recent_blockhash: Hash,
) -> Result<Signature, anyhow::Error> {
    if percent == 0 || percent > 100 {
        return Err(anyhow!("Percentage must be between 1 and 100"));
    }

    let (balance_u64, _) = get_token_balance(rpc.as_ref(), payer.as_ref(), &mint).await?;
    let amount = balance_u64 * percent / 100;
    sell(rpc, payer, mint, bonding_curve_creator, Some(amount), priority_fee, recent_blockhash).await
}

pub async fn sell_by_percent_with_tip(
    rpc: Arc<SolanaRpcClient>,
    fee_clients: Vec<Arc<FeeClient>>,
    payer: Arc<Keypair>,
    mint: Pubkey,
    bonding_curve_creator: Pubkey,
    percent: u64,
    priority_fee: PriorityFee,
    recent_blockhash: Hash,
) -> Result<(), anyhow::Error> {
    if percent == 0 || percent > 100 {
        return Err(anyhow!("Percentage must be between 1 and 100"));
    }

    let (balance_u64, _) = get_token_balance(rpc.as_ref(), payer.as_ref(), &mint).await?;
    let amount = balance_u64 * percent / 100;
    sell_with_tip(fee_clients, payer, mint, bonding_curve_creator, Some(amount), priority_fee, recent_blockhash).await
}

/// Sell tokens using Jito
pub async fn sell_with_tip(
    fee_clients: Vec<Arc<FeeClient>>,
    payer: Arc<Keypair>,
    mint: Pubkey,
    bonding_curve_creator: Pubkey,
    amount_token: Option<u64>,
    priority_fee: PriorityFee,
    recent_blockhash: Hash,
) -> Result<(), anyhow::Error> {
    let start_time = Instant::now();

    let mut transactions = vec![];
    let instructions = build_sell_instructions(payer.clone(), mint.clone(), bonding_curve_creator, amount_token).await?;

    for fee_client in fee_clients.clone() {
        let payer = payer.clone();
        let priority_fee = priority_fee.clone();
        let tip_account = fee_client.get_tip_account().await.map_err(|e| anyhow!(e.to_string()))?;
        let tip_account = Arc::new(Pubkey::from_str(&tip_account).map_err(|e| anyhow!(e))?);

        let transaction = build_sell_transaction_with_tip(tip_account, payer, priority_fee, instructions.clone(), recent_blockhash).await?;
        transactions.push(transaction);
    }

    let mut handles = vec![];
    for i in 0..fee_clients.len() {
        let fee_client = fee_clients[i].clone();
        let transaction = transactions[i].clone();
        let handle: JoinHandle<Result<(), anyhow::Error>> = tokio::spawn(async move {
            fee_client.send_transaction(&transaction).await?;
            println!("index: {}, Total Jito sell operation time: {:?}ms", i, start_time.elapsed().as_millis());
            Ok(())
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

    println!("Total Jito sell operation time: {:?}ms", start_time.elapsed().as_millis());
    Ok(())
}

pub async fn build_sell_transaction(
    payer: Arc<Keypair>,
    priority_fee: PriorityFee,
    build_instructions: Vec<Instruction>,
    recent_blockhash: Hash,
) -> Result<Transaction, anyhow::Error> {
    let mut instructions = vec![
        ComputeBudgetInstruction::set_compute_unit_price(priority_fee.unit_price),
        ComputeBudgetInstruction::set_compute_unit_limit(priority_fee.unit_limit),
    ];

    instructions.extend(build_instructions);

    let transaction = Transaction::new_signed_with_payer(
        &instructions,
        Some(&payer.pubkey()),
        &[payer.as_ref()],
        recent_blockhash,
    );

    Ok(transaction)
}

pub async fn build_sell_transaction_with_tip(
    tip_account: Arc<Pubkey>,
    payer: Arc<Keypair>,
    priority_fee: PriorityFee,
    build_instructions: Vec<Instruction>,
    blockhash: Hash,
) -> Result<VersionedTransaction, anyhow::Error> {
    let mut instructions = vec![
        ComputeBudgetInstruction::set_compute_unit_price(priority_fee.unit_price),
        ComputeBudgetInstruction::set_compute_unit_limit(priority_fee.unit_limit),
        system_instruction::transfer(
            &payer.pubkey(),
            &tip_account,
            sol_to_lamports(priority_fee.sell_tip_fee),
        ),
    ];

    instructions.extend(build_instructions);

    let v0_message: v0::Message =
        v0::Message::try_compile(&payer.pubkey(), &instructions, &[], blockhash)?;
    let versioned_message: VersionedMessage = VersionedMessage::V0(v0_message);

    let transaction = VersionedTransaction::try_new(versioned_message, &[&payer])?;

    Ok(transaction)
}

pub async fn build_sell_instructions(
    payer: Arc<Keypair>,
    mint: Pubkey,
    bonding_curve_creator: Pubkey,
    amount_token: Option<u64>,
) -> Result<Vec<Instruction>, anyhow::Error> {
    let amount = amount_token.ok_or(anyhow!("Amount cannot be none"))?;
    if amount == 0 {
        return Err(anyhow!("Amount cannot be zero"));
    }

    let ata = get_associated_token_address(&payer.pubkey(), &mint);
    let bonding_curve_pda = get_bonding_curve_pda(&mint).unwrap();
    let creator_vault_pda = get_creator_vault_pda(&bonding_curve_creator).unwrap();

    let instructions = vec![
        instruction::sell(
            payer.as_ref(),
            &mint,
            &bonding_curve_pda,
            &creator_vault_pda,
            &FEE_RECIPIENT,
            instruction::Sell {
                _amount: amount,
                _min_sol_output: 0,
            },
        ),
    
        close_account(
            &spl_token::ID,
            &ata,
            &payer.pubkey(),
            &payer.pubkey(),
            &[&payer.pubkey()],
        )?
    ];

    Ok(instructions)
}
