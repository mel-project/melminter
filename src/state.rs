use anyhow::Context;
use melwallet_client::WalletClient;
use serde::{Deserialize, Serialize};
use themelio_nodeprot::ValClient;
use themelio_stf::{melpow, CoinData, Denom, PoolKey, Transaction, TxKind};

#[derive(Debug, Clone)]
pub struct MintState {
    wallet: WalletClient,
    client: ValClient,
}

#[derive(Serialize, Deserialize)]
struct PrepareReq {
    signing_key: String,
    outputs: Vec<CoinData>,
}

impl MintState {
    pub fn new(wallet: WalletClient, client: ValClient) -> Self {
        Self { wallet, client }
    }

    async fn prepare_dummy(&self) -> surf::Result<Transaction> {
        let my_address = self.wallet.summary().await?.address;
        let res = self
            .wallet
            .prepare_transaction(
                TxKind::DoscMint,
                vec![],
                vec![CoinData {
                    covhash: my_address,
                    denom: Denom::Mel,
                    value: 1,
                    additional_data: vec![],
                }],
                None,
                vec![0u8; 65536],
                vec![],
            )
            .await?;
        Ok(res)
    }

    /// Creates a partially-filled-in transaction, with the given difficulty, that's neither signed nor feed. The caller should fill in the DOSC output.
    pub async fn mint_transaction(&self, difficulty: usize) -> surf::Result<(Transaction, u64)> {
        let mut transaction = self.prepare_dummy().await?;
        let tip_cdh = self
            .client
            .snapshot()
            .await?
            .get_coin(transaction.inputs[0])
            .await?
            .context("dummy transaction's input spent from behind our back")?;
        // log::debug!("tip_cdh = {:#?}", tip_cdh);
        let snapshot = self.client.snapshot().await?;
        // log::debug!("snapshot height = {}", snapshot.current_header().height);
        let tip_header_hash = snapshot
            .get_history(tip_cdh.height)
            .await?
            .context("history not found")?
            .hash();
        let chi = tmelcrypt::hash_keyed(
            &tip_header_hash,
            &stdcode::serialize(&transaction.inputs[0]).unwrap(),
        );
        let proof = smol::unblock(move || melpow::Proof::generate(&chi, difficulty)).await;
        let difficulty = difficulty as u32;
        let proof_bytes = proof.to_bytes();
        assert!(melpow::Proof::from_bytes(&proof_bytes)
            .unwrap()
            .verify(&chi, difficulty as usize));

        transaction.data = stdcode::serialize(&(difficulty, proof_bytes)).unwrap();
        transaction.outputs.clear();

        Ok((transaction, tip_cdh.height))
    }

    /// Sends a transaction out. What this actually does is to re-prepare another transaction with the same inputs, outputs, and data, so that the wallet can sign it properly.
    pub async fn send_resigned_transaction(&self, transaction: Transaction) -> surf::Result<()> {
        let resigned = self
            .wallet
            .prepare_transaction(
                TxKind::DoscMint,
                transaction.inputs.clone(),
                transaction.outputs.clone(),
                None,
                transaction.data.clone(),
                vec![Denom::NomDosc],
            )
            .await?;
        let txhash = self.wallet.send_tx(resigned).await?;
        self.wallet.wait_transaction(txhash).await?;
        Ok(())
    }

    /// Converts a given number of doscs to mel.
    pub async fn convert_doscs(&self, doscs: u128) -> surf::Result<()> {
        let my_address = self.wallet.summary().await?.address;
        let tx = self
            .wallet
            .prepare_transaction(
                TxKind::Swap,
                vec![],
                vec![CoinData {
                    covhash: my_address,
                    value: doscs,
                    denom: Denom::NomDosc,
                    additional_data: vec![],
                }],
                None,
                PoolKey::new(Denom::Mel, Denom::NomDosc).to_bytes(),
                vec![],
            )
            .await?;
        let txhash = self.wallet.send_tx(tx).await?;
        self.wallet.wait_transaction(txhash).await?;
        Ok(())
    }
}
