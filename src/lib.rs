pub mod accounts;
pub mod constants;
pub mod error;
pub mod instruction;
pub mod grpc;
pub mod common;
pub mod ipfs;
pub mod swqos;
pub mod pumpfun;

use std::sync::Arc;

use swqos::{FeeClient, JitoClient, NextBlockClient, ZeroSlotClient};
use rustls::crypto::{ring::default_provider, CryptoProvider};
use solana_hash::Hash;
use solana_sdk::{
    commitment_config::CommitmentConfig,
    pubkey::Pubkey,
    signature::{Keypair, Signer},
};
use solana_sdk::signature::Signature;
use tokio::sync::RwLock;
use anyhow::anyhow;
use common::{logs_data::TradeInfo, logs_events::PumpfunEvent, logs_subscribe, Cluster, PriorityFee, SolanaRpcClient};
use common::logs_subscribe::SubscriptionHandle;
use crate::pumpfun::common::get_bonding_curve_account;

pub struct PumpFun {
    pub payer: Arc<Keypair>,
    pub rpc: Arc<SolanaRpcClient>,
    pub fee_clients: Vec<Arc<FeeClient>>,
    pub priority_fee: PriorityFee,
    pub cluster: Cluster,
    pub blockhash_cache: Arc<RwLock<Hash>>
}

impl Clone for PumpFun {
    fn clone(&self) -> Self {
        Self {
            payer: self.payer.clone(),
            rpc: self.rpc.clone(),
            fee_clients: self.fee_clients.clone(),
            priority_fee: self.priority_fee.clone(),
            cluster: self.cluster.clone(),
            blockhash_cache: self.blockhash_cache.clone(),
        }
    }
}

impl PumpFun {
    #[inline]
    pub async fn new(
        payer: Arc<Keypair>,
        cluster: &Cluster,
    ) -> Self {
        if CryptoProvider::get_default().is_none() {
            let _ = default_provider()
                .install_default()
                .map_err(|e| anyhow::anyhow!("Failed to install crypto provider: {:?}", e));
        }

        let rpc = Arc::new(SolanaRpcClient::new_with_commitment(
            cluster.clone().rpc_url,
            cluster.clone().commitment
        ));
        let blockhash_cache = Arc::new(RwLock::new(rpc.get_latest_blockhash().await.unwrap()));

        let rpc_clone = rpc.clone();
        let blockhash_cache_clone = blockhash_cache.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;
                *blockhash_cache_clone.write().await = rpc_clone.get_latest_blockhash().await.unwrap();
            }
        });

        let mut fee_clients: Vec<Arc<FeeClient>> = vec![];
        if cluster.clone().use_jito {
            let jito_client = JitoClient::new(
                cluster.clone().rpc_url, 
                cluster.clone().block_engine_url
            ).await.expect("Failed to create Jito client");

            fee_clients.push(Arc::new(jito_client));
        }

        if cluster.clone().use_zeroslot {
            let zeroslot_client = ZeroSlotClient::new(
                cluster.clone().rpc_url, 
                cluster.clone().zeroslot_url,
                cluster.clone().zeroslot_auth_token
            );

            fee_clients.push(Arc::new(zeroslot_client));
        }

        if cluster.clone().use_nextblock {
            let nextblock_client = NextBlockClient::new(
                cluster.clone().rpc_url,
                cluster.clone().nextblock_url,
                cluster.clone().nextblock_auth_token
            );

            fee_clients.push(Arc::new(nextblock_client));
        }

        Self {
            payer,
            rpc: rpc.clone(),
            fee_clients,
            priority_fee: cluster.clone().priority_fee,
            cluster: cluster.clone(),
            blockhash_cache: blockhash_cache.clone(),
        }
    }
    
    /// Buy tokens
    pub async fn buy(
        &self,
        mint: Pubkey,
        creator: Pubkey,
        buy_token_amount: u64,
        max_amount_sol: u64,
        slippage_basis_points: Option<u64>,
    ) -> Result<Signature, anyhow::Error> {
        pumpfun::buy::buy(
            self.rpc.clone(),
            self.payer.clone(),
            mint,
            creator,
            buy_token_amount,
            max_amount_sol,
            slippage_basis_points,
            self.priority_fee.clone(),
            self.blockhash_cache.read().await.clone()
        ).await
    }

    /// Buy tokens using Jito
    pub async fn buy_with_tip(
        &self,
        mint: Pubkey,
        creator: Pubkey,
        buy_token_amount: u64,
        amount_sol: u64,
        slippage_basis_points: Option<u64>,
    ) -> Result<(), anyhow::Error> {
        pumpfun::buy::buy_with_tip(
            self.fee_clients.clone(),
            self.payer.clone(),
            mint,
            creator,
            buy_token_amount,
            amount_sol,
            slippage_basis_points,
            self.priority_fee.clone(),
            self.blockhash_cache.read().await.clone(),
        ).await
    }

    /// Sell tokens
    pub async fn sell(
        &self,
        mint: Pubkey,
        bonding_curve_creator: Pubkey,
        amount_token: Option<u64>,
    ) -> Result<Signature, anyhow::Error> {
        pumpfun::sell::sell(
            self.rpc.clone(),
            self.payer.clone(),
            mint.clone(),
            bonding_curve_creator,
            amount_token,
            self.priority_fee.clone(),
            self.blockhash_cache.read().await.clone(),
        ).await
    }

    /// Sell tokens by percentage
    pub async fn sell_by_percent(
        &self,
        mint: Pubkey,
        bonding_curve_creator: Pubkey,
        percent: u64,
    ) -> Result<Signature, anyhow::Error> {
        pumpfun::sell::sell_by_percent(
            self.rpc.clone(),
            self.payer.clone(),
            mint.clone(),
            bonding_curve_creator,
            percent,
            self.priority_fee.clone(),
            self.blockhash_cache.read().await.clone(),
        ).await
    }

    pub async fn sell_by_percent_with_tip(
        &self,
        mint: Pubkey,
        bonding_curve_creator: Pubkey,
        percent: u64,
    ) -> Result<(), anyhow::Error> {
        pumpfun::sell::sell_by_percent_with_tip(
            self.rpc.clone(),
            self.fee_clients.clone(),
            self.payer.clone(),
            mint,
            bonding_curve_creator,
            percent,
            self.priority_fee.clone(),
            self.blockhash_cache.read().await.clone(),
        ).await
    }

    /// Sell tokens using Jito
    pub async fn sell_with_tip(
        &self,
        mint: Pubkey,
        bonding_curve_creator: Pubkey,
        amount_token: Option<u64>,
    ) -> Result<(), anyhow::Error> {
        pumpfun::sell::sell_with_tip(
            self.fee_clients.clone(),
            self.payer.clone(),
            bonding_curve_creator,
            mint,
            amount_token,
            self.priority_fee.clone(),
            self.blockhash_cache.read().await.clone(),
        ).await
    }

    #[inline]
    pub async fn tokens_subscription<F>(
        &self,
        ws_url: &str,
        commitment: CommitmentConfig,
        callback: F,
        bot_wallet: Option<Pubkey>,
    ) -> Result<SubscriptionHandle, Box<dyn std::error::Error>>
    where
        F: Fn(PumpfunEvent) + Send + Sync + 'static,
    {
        logs_subscribe::tokens_subscription(ws_url, commitment, callback, bot_wallet).await
    }

    #[inline]
    pub async fn stop_subscription(&self, subscription_handle: SubscriptionHandle) {
        subscription_handle.shutdown().await;
    }

    #[inline]
    pub async fn get_sol_balance(&self, payer: &Pubkey) -> Result<u64, anyhow::Error> {
        pumpfun::common::get_sol_balance(&self.rpc, payer).await
    }

    #[inline]
    pub async fn get_payer_sol_balance(&self) -> Result<u64, anyhow::Error> {
        pumpfun::common::get_sol_balance(&self.rpc, &self.payer.pubkey()).await
    }

    #[inline]
    pub async fn get_token_balance(&self, payer: &Pubkey, mint: &Pubkey) -> Result<u64, anyhow::Error> {
        println!("get_token_balance payer: {}, mint: {}, cluster: {}", payer, mint, self.cluster.rpc_url);
        pumpfun::common::get_token_balance(&self.rpc, payer, mint).await
    }

    #[inline]
    pub async fn get_payer_token_balance(&self, mint: &Pubkey) -> Result<u64, anyhow::Error> {
        pumpfun::common::get_token_balance(&self.rpc, &self.payer.pubkey(), mint).await
    }

    #[inline]
    pub fn get_payer_pubkey(&self) -> Pubkey {
        self.payer.pubkey()
    }

    #[inline]
    pub fn get_payer(&self) -> &Keypair {
        self.payer.as_ref()
    }

    #[inline]
    pub fn get_token_price(&self,virtual_sol_reserves: u64, virtual_token_reserves: u64) -> f64 {
        pumpfun::common::get_token_price(virtual_sol_reserves, virtual_token_reserves)
    }

    #[inline]
    pub fn get_buy_price(&self, amount: u64, trade_info: &TradeInfo) -> u64 {
        pumpfun::common::get_buy_price(amount, trade_info)
    }

    #[inline]
    pub fn get_sell_price(&self, amount: u64, trade_info: &TradeInfo) -> u64 {
        pumpfun::common::get_sell_price(amount, trade_info)
    }

    #[inline]
    pub async fn get_sell_price_from_bonding_curve(&self, mint: &Pubkey, amount: u64) -> Result<u64, anyhow::Error> {
        let (bonding_curve_account, _) = get_bonding_curve_account(&self.rpc, mint).await
            .map_err(|e| anyhow!(e))?;
        Ok(pumpfun::common::get_sell_price_from_bonding_curve(amount, &bonding_curve_account))
    }

    #[inline]
    pub async fn transfer_sol(&self, payer: &Keypair, receive_wallet: &Pubkey, amount: u64) -> Result<(), anyhow::Error> {
        pumpfun::common::transfer_sol(&self.rpc, payer, receive_wallet, amount).await
    }
}
