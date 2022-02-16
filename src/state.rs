use anyhow::Context;
use melwallet_client::WalletClient;
use serde::{Deserialize, Serialize};
use stdcode::StdcodeSerializeExt;
use themelio_nodeprot::ValClient;
use themelio_stf::{melpow, PoolKey};
use themelio_structs::{
    BlockHeight, CoinData, CoinDataHeight, CoinID, CoinValue, Denom, Transaction, TxKind,
};

#[derive(Clone)]
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

    /// Gets a list of "seed" coins available. If none are available, generates some and blocks until they are available.
    async fn get_seeds(&self) -> surf::Result<Vec<CoinID>> {
        let my_address = self.wallet.summary().await?.address;
        loop {
            let toret = self.get_seeds_raw().await?;
            if !toret.is_empty() {
                return Ok(toret);
            }
            // generate a bunch of custom-token utxos
            let tx = self
                .wallet
                .prepare_transaction(
                    TxKind::Normal,
                    vec![],
                    std::iter::repeat_with(|| CoinData {
                        covhash: my_address,
                        denom: Denom::NewCoin,
                        value: CoinValue(1),
                        additional_data: vec![],
                    })
                    .take(64)
                    .collect(),
                    vec![],
                    vec![],
                    vec![],
                )
                .await?;
            let sent_hash = self.wallet.send_tx(tx).await?;
            log::info!("no seed coins, creating a bunch with {:?}...", sent_hash);
            self.wallet.wait_transaction(sent_hash).await?;
        }
    }

    async fn get_seeds_raw(&self) -> surf::Result<Vec<CoinID>> {
        let dumped = self.wallet.dump_wallet().await?.full;
        Ok(dumped
            .unspent_coins
            .into_iter()
            .filter_map(|(k, v)| {
                if matches!(v.coin_data.denom, Denom::Custom(_)) {
                    Some(k)
                } else {
                    None
                }
            })
            .collect())
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
                    value: CoinValue(1),
                    additional_data: vec![],
                }],
                vec![],
                vec![0u8; 65536],
                vec![],
            )
            .await?;
        Ok(res)
    }

    /// Creates a partially-filled-in transaction, with the given difficulty, that's neither signed nor feed. The caller should fill in the DOSC output.
    pub async fn mint_batch(
        &self,
        difficulty: usize,
    ) -> surf::Result<Vec<(CoinID, CoinDataHeight, Vec<u8>)>> {
        let seeds = self.get_seeds().await?;
        let mut proofs = Vec::new();
        for (idx, seed) in seeds
            .iter()
            .copied()
            .take(num_cpus::get_physical())
            .enumerate()
        {
            let tip_cdh = self
                .client
                .snapshot()
                .await?
                .get_coin(seed)
                .await?
                .context("transaction's input spent from behind our back")?;
            // log::debug!("tip_cdh = {:#?}", tip_cdh);
            let snapshot = self.client.snapshot().await?;
            // log::debug!("snapshot height = {}", snapshot.current_header().height);
            let tip_header_hash = snapshot
                .get_older(tip_cdh.height)
                .await?
                .current_header()
                .hash();
            let chi = tmelcrypt::hash_keyed(&tip_header_hash, &seed.stdcode());
            let proof_fut = smolscale::spawn(async move {
                (
                    tip_cdh,
                    melpow::Proof::generate_with_progress(&chi, difficulty, |progress| {
                        if fastrand::f64() < 0.0001 {
                            log::info!("thread {} at {:.3}%", idx, progress * 100.0);
                        }
                    }),
                )
            });
            proofs.push(proof_fut);
        }
        let mut out = vec![];
        for (seed, proof) in seeds.into_iter().zip(proofs.into_iter()) {
            let result = proof.await;
            out.push((seed, result.0, result.1.to_bytes()))
        }
        Ok(out)
    }

    /// Sends a transaction.
    pub async fn send_mint_transaction(
        &self,
        seed: CoinID,
        proof: Vec<u8>,
        ergs: CoinValue,
    ) -> surf::Result<()> {
        let own_cov = self.wallet.summary().await?.address;
        let tx = self
            .wallet
            .prepare_transaction(
                TxKind::DoscMint,
                vec![seed],
                vec![CoinData {
                    denom: Denom::Erg,
                    value: ergs,
                    additional_data: vec![],
                    covhash: own_cov,
                }],
                vec![],
                proof,
                vec![Denom::Erg],
            )
            .await?;
        let txhash = self.wallet.send_tx(tx).await?;
        self.wallet.wait_transaction(txhash).await?;
        Ok(())
    }

    // /// Sends a transaction out. What this actually does is to re-prepare another transaction with the same inputs, outputs, and data, so that the wallet can sign it properly.
    // pub async fn send_resigned_transaction(&self, transaction: Transaction) -> surf::Result<()> {
    //     let resigned = self
    //         .wallet
    //         .prepare_transaction(
    //             TxKind::DoscMint,
    //             transaction.inputs.clone(),
    //             transaction.outputs.clone(),
    //             vec![],
    //             transaction.data.clone(),
    //             vec![Denom::Erg],
    //         )
    //         .await?;
    //     let txhash = self.wallet.send_tx(resigned).await?;
    //     self.wallet.wait_transaction(txhash).await?;
    //     Ok(())
    // }

    /// Converts a given number of doscs to mel.
    pub async fn convert_doscs(&self, doscs: CoinValue) -> surf::Result<()> {
        let my_address = self.wallet.summary().await?.address;
        let tx = self
            .wallet
            .prepare_transaction(
                TxKind::Swap,
                vec![],
                vec![CoinData {
                    covhash: my_address,
                    value: doscs,
                    denom: Denom::Erg,
                    additional_data: vec![],
                }],
                vec![],
                PoolKey::new(Denom::Mel, Denom::Erg).to_bytes(),
                vec![],
            )
            .await?;
        let txhash = self.wallet.send_tx(tx).await?;
        self.wallet.wait_transaction(txhash).await?;
        Ok(())
    }

    /// Converts ERG to MEL
    pub async fn erg_to_mel(&self, ergs: CoinValue) -> surf::Result<CoinValue> {
        let mut pool = self
            .client
            .snapshot()
            .await?
            .get_pool(PoolKey::mel_and(Denom::Erg))
            .await?
            .expect("no erg/mel pool");
        Ok(pool.swap_many(ergs.0, 0).1.into())
    }
}
