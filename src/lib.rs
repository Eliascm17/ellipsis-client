use anyhow::anyhow;
use async_trait::async_trait;
use itertools::Itertools;
use solana_client::{rpc_client::RpcClient, rpc_config::RpcSendTransactionConfig};
use solana_program::{
    hash::Hash,
    instruction::Instruction,
    program_error::ProgramError,
    pubkey::Pubkey,
    rent::Rent,
    sysvar::{self},
};
use solana_program_test::BanksClient;
use solana_program_test::BanksClientError;
use solana_sdk::{
    account::Account,
    commitment_config::{CommitmentConfig, CommitmentLevel},
    signature::{Keypair, Signature},
    signer::Signer,
    transaction::Transaction,
    transport::TransportError,
};
use std::sync::{Arc, PoisonError};
use thiserror::Error;
use tokio::sync::RwLock;

pub type LightweightClientResult<T = ()> = std::result::Result<T, LightweightClientError>;

#[derive(Error, Debug)]
pub enum LightweightClientError {
    #[error("Public keys expected to match but do not")]
    PublicKeyMismatch,
    #[error("Action requires admin key")]
    RequiresAdmin,
    #[error("Solana client error")]
    SolanaClient(#[from] solana_client::client_error::ClientError),
    #[error("Some other error")]
    Other(#[from] anyhow::Error),
    #[error("Transaction Failed")]
    TransactionFailed,
    #[error("Transport Error")]
    TransportError(#[from] TransportError),
    #[error("Program Error")]
    ProgramError(#[from] ProgramError),
}

impl From<Box<dyn std::error::Error>> for LightweightClientError {
    fn from(e: Box<dyn std::error::Error>) -> Self {
        LightweightClientError::Other(anyhow::Error::msg(e.to_string()))
    }
}

impl<T> From<PoisonError<T>> for LightweightClientError {
    fn from(e: PoisonError<T>) -> Self {
        LightweightClientError::Other(anyhow::Error::msg(e.to_string()))
    }
}

impl From<BanksClientError> for LightweightClientError {
    fn from(e: BanksClientError) -> Self {
        LightweightClientError::Other(anyhow::Error::msg(e.to_string()))
    }
}

impl From<std::io::Error> for LightweightClientError {
    fn from(e: std::io::Error) -> Self {
        LightweightClientError::TransportError(TransportError::from(e))
    }
}

pub fn clone_keypair(keypair: &Keypair) -> Keypair {
    Keypair::from_bytes(&keypair.to_bytes()).unwrap()
}

#[async_trait]
pub trait ClientSubset {
    async fn process_transaction(
        &self,
        mut tx: Transaction,
        signers: &Vec<&Keypair>,
    ) -> LightweightClientResult<Signature>;
    async fn fetch_latest_blockhash(&self) -> LightweightClientResult<Hash>;
    async fn fetch_account(&self, key: Pubkey) -> LightweightClientResult<Account>;
}

pub trait ClientSubsetSync {
    fn process_transaction(
        &self,
        tx: Transaction,
        signers: &Vec<&Keypair>,
    ) -> LightweightClientResult<Signature>;
    fn fetch_latest_blockhash(&self) -> LightweightClientResult<Hash>;
    fn fetch_account(&self, key: Pubkey) -> LightweightClientResult<Account>;
}

pub struct LightweightSolanaClient {
    pub client: Arc<dyn ClientSubset + 'static + Sync + Send>,
    rent: Rent,
    pub payer: Keypair,
}

impl LightweightSolanaClient {
    pub async fn from_banks(
        client: &BanksClient,
        payer: &Keypair,
    ) -> std::result::Result<Self, LightweightClientError> {
        let mut client = client.clone();
        let rent = client.get_rent().await?;
        Ok(Self {
            rent,
            client: Arc::new(RwLock::new(client)),
            payer: clone_keypair(payer),
        })
    }

    pub fn from_rpc(
        rpc: RpcClient,
        payer: &Keypair,
    ) -> std::result::Result<Self, LightweightClientError> {
        let rent_account = rpc
            .get_account_with_commitment(&sysvar::rent::id(), CommitmentConfig::confirmed())?
            .value
            .ok_or(anyhow!("Failed to fetch rent sysvar"))?;
        let rent = bincode::deserialize(&*rent_account.data).map_err(|e| anyhow::Error::from(e))?;
        Ok(Self {
            client: Arc::new(Arc::new(rpc)),
            rent,
            payer: clone_keypair(payer),
        })
    }

    pub async fn sign_send_instructions_with_payer(
        &self,
        instructions: Vec<Instruction>,
        mut signers: Vec<&Keypair>, // todo: use slice
    ) -> LightweightClientResult<Signature> {
        signers.insert(0, &self.payer);
        self.client
            .process_transaction(
                Transaction::new_with_payer(&instructions, Some(&self.payer.pubkey())),
                &signers,
            )
            .await
    }

    pub async fn sign_send_instructions(
        &self,
        instructions: Vec<Instruction>,
        mut signers: Vec<&Keypair>, // todo: use slice
    ) -> LightweightClientResult<Signature> {
        let payer = if signers.len() > 0 {
            signers[0].pubkey()
        } else {
            signers.insert(0, &self.payer);
            self.payer.pubkey()
        };
        self.client
            .process_transaction(
                Transaction::new_with_payer(&instructions, Some(&payer)),
                &signers,
            )
            .await
    }

    pub async fn get_latest_blockhash(&self) -> LightweightClientResult<Hash> {
        self.client.fetch_latest_blockhash().await
    }

    pub fn rent_exempt(&self, size: usize) -> u64 {
        self.rent.minimum_balance(size) as u64
    }

    pub async fn get_account(&self, key: Pubkey) -> LightweightClientResult<Account> {
        self.client.fetch_account(key).await
    }
}

#[async_trait]
impl ClientSubset for Arc<RpcClient> {
    async fn process_transaction(
        &self,
        tx: Transaction,
        signers: &Vec<&Keypair>,
    ) -> LightweightClientResult<Signature> {
        let signers_owned = signers.into_iter().map(|&i| clone_keypair(i)).collect_vec();
        let signers = signers_owned.iter().collect();
        self.process_transaction(tx, &signers)
            .await
            .map_err(|e| LightweightClientError::Other(anyhow::Error::msg(e.to_string())))
    }

    async fn fetch_latest_blockhash(&self) -> std::result::Result<Hash, LightweightClientError> {
        self.fetch_latest_blockhash()
            .await
            .map_err(|e| LightweightClientError::Other(anyhow::Error::msg(e.to_string())))
    }

    async fn fetch_account(&self, key: Pubkey) -> LightweightClientResult<Account> {
        self.fetch_account(key)
            .await
            .map_err(|e| LightweightClientError::Other(anyhow::Error::msg(e.to_string())))
    }
}

impl ClientSubsetSync for RpcClient {
    fn process_transaction(
        &self,
        mut tx: Transaction,
        signers: &Vec<&Keypair>,
    ) -> LightweightClientResult<Signature> {
        tx.partial_sign(signers, self.get_latest_blockhash()?);
        self.send_and_confirm_transaction_with_spinner_and_config(
            &tx,
            CommitmentConfig::confirmed(),
            RpcSendTransactionConfig {
                min_context_slot: None,
                skip_preflight: true,
                preflight_commitment: None,
                encoding: None,
                max_retries: None,
            },
        )?;
        Ok(tx.signatures[0])
    }

    fn fetch_latest_blockhash(&self) -> std::result::Result<Hash, LightweightClientError> {
        Ok(self
            .get_latest_blockhash_with_commitment(CommitmentConfig::processed())
            .map(|(hash, _)| hash)?)
    }

    fn fetch_account(&self, key: Pubkey) -> std::result::Result<Account, LightweightClientError> {
        Ok(self
            .get_account_with_commitment(&key, CommitmentConfig::processed())?
            .value
            .ok_or(anyhow!("Failed to get account"))?)
    }
}

#[async_trait]
impl ClientSubset for RwLock<BanksClient> {
    async fn process_transaction(
        &self,
        mut tx: Transaction,
        signers: &Vec<&Keypair>,
    ) -> LightweightClientResult<Signature> {
        tx.partial_sign(signers, self.fetch_latest_blockhash().await?);
        let sig = tx.signatures[0];
        self.write()
            .await
            .process_transaction_with_commitment(tx, CommitmentLevel::Confirmed)
            .await?;
        Ok(sig)
    }

    async fn fetch_latest_blockhash(&self) -> std::result::Result<Hash, LightweightClientError> {
        self.write()
            .await
            .get_latest_blockhash()
            .await
            .map_err(LightweightClientError::from)
    }

    async fn fetch_account(
        &self,
        key: Pubkey,
    ) -> std::result::Result<Account, LightweightClientError> {
        self.write()
            .await
            .get_account_with_commitment(key, CommitmentLevel::Confirmed)
            .await?
            .ok_or(anyhow!("Failed to get account").into())
    }
}

///////////// Non-interesting impls  ////////////////////