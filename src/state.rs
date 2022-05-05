use std::sync::Arc;

use anyhow::Context;
use melwallet_client::WalletClient;
use serde::{Deserialize, Serialize};
use stdcode::StdcodeSerializeExt;
use themelio_nodeprot::ValClient;
use themelio_stf::{PoolKey, Tip910MelPowHash};
use themelio_structs::{CoinData, CoinDataHeight, CoinID, CoinValue, Denom, TxHash, TxKind};

use crate::repeat_fallible;

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

    /// Generates a list of "seed" coins.
    pub async fn generate_seeds(&self, threads: usize) -> surf::Result<()> {
        let my_address = self.wallet.summary().await?.address;
        loop {
            let toret = self.get_seeds_raw().await?;
            if toret.len() >= threads {
                return Ok(());
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
                    .take(threads)
                    .collect(),
                    vec![],
                    vec![],
                    vec![],
                )
                .await?;
            let sent_hash = self.wallet.send_tx(tx).await?;
            self.wallet.wait_transaction(sent_hash).await?;
        }
    }

    async fn get_seeds_raw(&self) -> surf::Result<Vec<CoinID>> {
        let unspent_coins = self.wallet.get_coins().await?;

        Ok(unspent_coins
            .iter()
            .filter_map(|(k, v)| {
                if matches!(v.denom, Denom::Custom(_)) {
                    Some((k, v))
                } else {
                    None
                }
            })
            .map(|d| *d.0)
            .collect())
    }

    /// Creates a partially-filled-in transaction, with the given difficulty, that's neither signed nor feed. The caller should fill in the DOSC output.
    pub async fn mint_batch(
        &self,
        difficulty: usize,
        on_progress: impl Fn(usize, f64) + Sync + Send + 'static,
        threads: usize,
    ) -> surf::Result<Vec<(CoinID, CoinDataHeight, Vec<u8>)>> {
        let seeds = self.get_seeds_raw().await?;
        let on_progress = Arc::new(on_progress);
        let mut proofs = Vec::new();
        for (idx, seed) in seeds.iter().copied().take(threads).enumerate() {
            let tip_cdh =
                repeat_fallible(|| async { self.client.snapshot().await?.get_coin(seed).await })
                    .await
                    .context("transaction's input spent from behind our back")?;
            // log::debug!("tip_cdh = {:#?}", tip_cdh);
            let snapshot = self.client.snapshot().await?;
            // log::debug!("snapshot height = {}", snapshot.current_header().height);
            let tip_header_hash = repeat_fallible(|| snapshot.get_older(tip_cdh.height))
                .await
                .current_header()
                .hash();
            let chi = tmelcrypt::hash_keyed(&tip_header_hash, &seed.stdcode());
            let on_progress = on_progress.clone();
            // let core_ids = core_affinity::get_core_ids().unwrap();
            // let core_id = core_ids[idx % core_ids.len()];
            let proof_fut = std::thread::spawn(move || {
                // core_affinity::set_for_current(core_id);
                (
                    tip_cdh,
                    melpow::Proof::generate_with_progress(
                        &chi,
                        difficulty,
                        |progress| {
                            if fastrand::f64() < 0.1 {
                                on_progress(idx, progress)
                            }
                        },
                        Tip910MelPowHash,
                    ),
                )
            });
            proofs.push(proof_fut);
        }
        let mut out = vec![];
        for (seed, proof) in seeds.into_iter().zip(proofs.into_iter()) {
            let result = smol::unblock(move || proof.join().unwrap()).await;
            out.push((seed, result.0, result.1.to_bytes()))
        }
        Ok(out)
    }

    /// Sends a transaction.
    pub async fn send_mint_transaction(
        &self,
        seed: CoinID,
        difficulty: usize,
        proof: Vec<u8>,
        ergs: CoinValue,
    ) -> surf::Result<TxHash> {
        self.wallet.unlock(None).await?;
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
                (difficulty, proof).stdcode(),
                vec![Denom::Erg],
            )
            .await?;
        let txhash = self.wallet.send_tx(tx).await?;
        Ok(txhash)
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
