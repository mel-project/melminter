use std::{sync::Arc, time::Duration};

use anyhow::Context;
use melprot::Client;
use melstf::Tip910MelPowHash;
use melstructs::{
    Address, CoinData, CoinDataHeight, CoinID, CoinValue, Denom, PoolKey, TxHash, TxKind,
};

use melwalletd_prot::{types::PrepareTxArgs, MelwalletdClient};
use serde::{Deserialize, Serialize};
use stdcode::StdcodeSerializeExt;

use crate::repeat_fallible;

#[derive(Clone)]
pub struct MintState {
    daemon: Arc<MelwalletdClient>,
    wallet: String,
    client: Client,
}

#[derive(Serialize, Deserialize)]
struct PrepareReq {
    signing_key: String,
    outputs: Vec<CoinData>,
}

impl MintState {
    pub fn new(daemon: Arc<MelwalletdClient>, wallet: String, client: Client) -> Self {
        Self {
            daemon,
            wallet,
            client,
        }
    }

    /// Generates a list of "seed" coins.
    ///
    /// The minter needs to prove that it performed some computation between some time `t` and now (Something like `H(H(H(x)))`).
    /// where `x` is only known to it after a certain amount of time `t` has passed.
    ///
    /// To ensure that `x` is unique and NOT reusable to produce multiple proofs, so we
    /// calculate it with the following:
    /// 1. The spending of these "seed" coins The coins can only be spent
    /// once.
    /// 2. The block hash of the block containining the transaction that produced the first coin
    ///    being spent - this is a value that isn't known until that block is produced.
    pub async fn generate_seeds(&self, threads: usize) -> surf::Result<()> {
        let my_address = self
            .daemon
            .wallet_summary(self.wallet.clone())
            .await??
            .address;
        loop {
            let toret = self.get_seeds_raw().await?;
            if toret.len() >= threads {
                return Ok(());
            }
            // generate a bunch of custom-token utxos
            let tx = self
                .daemon
                .prepare_tx(
                    self.wallet.clone(),
                    PrepareTxArgs {
                        kind: TxKind::Normal,
                        inputs: vec![],
                        outputs: std::iter::repeat_with(|| CoinData {
                            covhash: my_address,
                            denom: Denom::NewCustom,
                            value: CoinValue(1),
                            additional_data: vec![].into(),
                        })
                        .take(threads)
                        .collect(),
                        covenants: vec![],
                        data: vec![],
                        nobalance: vec![],
                        fee_ballast: 100,
                    },
                )
                .await??;
            let sent_hash = self.daemon.send_tx(self.wallet.clone(), tx).await??;
            self.wait_tx(sent_hash).await?;
        }
    }

    async fn wait_tx(&self, txhash: TxHash) -> surf::Result<()> {
        while self
            .daemon
            .tx_status(self.wallet.clone(), txhash.0)
            .await??
            .context("no such")?
            .confirmed_height
            .is_none()
        {
            smol::Timer::after(Duration::from_secs(1)).await;
        }
        Ok(())
    }
    async fn get_seeds_raw(&self) -> surf::Result<Vec<CoinID>> {
        let unspent_coins = self.daemon.dump_coins(self.wallet.clone()).await??;

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
            let tip_cdh = repeat_fallible(|| async {
                self.client.latest_snapshot().await?.get_coin(seed).await
            })
            .await
            .context("transaction's input spent from behind our back")?;
            // log::debug!("tip_cdh = {:#?}", tip_cdh);
            let snapshot = self.client.latest_snapshot().await?;
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

    async fn address(&self) -> surf::Result<Address> {
        Ok(self
            .daemon
            .wallet_summary(self.wallet.clone())
            .await??
            .address)
    }

    /// Sends a transaction.
    pub async fn send_mint_transaction(
        &self,
        seed: CoinID,
        difficulty: usize,
        proof: Vec<u8>,
        ergs: CoinValue,
    ) -> surf::Result<TxHash> {
        self.daemon
            .unlock_wallet(self.wallet.clone(), "".into())
            .await??;
        let own_cov = self.address().await?;
        let tx = self
            .daemon
            .prepare_tx(
                self.wallet.clone(),
                PrepareTxArgs {
                    kind: TxKind::DoscMint,
                    inputs: vec![seed],
                    outputs: vec![CoinData {
                        denom: Denom::Erg,
                        value: ergs,
                        additional_data: vec![].into(),
                        covhash: own_cov,
                    }],
                    covenants: vec![],
                    data: (difficulty, proof).stdcode(),
                    nobalance: vec![Denom::Erg],
                    fee_ballast: 100,
                },
            )
            .await??;
        let txhash = self.daemon.send_tx(self.wallet.clone(), tx).await??;
        Ok(txhash)
    }

    /// Converts a given number of doscs to mel.
    pub async fn convert_doscs(&self, doscs: CoinValue) -> surf::Result<()> {
        let my_address = self.address().await?;
        let tx = self
            .daemon
            .prepare_tx(
                self.wallet.clone(),
                PrepareTxArgs {
                    kind: TxKind::Swap,
                    inputs: vec![],
                    outputs: vec![CoinData {
                        covhash: my_address,
                        value: doscs,
                        denom: Denom::Erg,
                        additional_data: vec![].into(),
                    }],
                    covenants: vec![],
                    data: PoolKey::new(Denom::Mel, Denom::Erg).to_bytes().into(),
                    nobalance: vec![],
                    fee_ballast: 100,
                },
            )
            .await??;
        let txhash = self.daemon.send_tx(self.wallet.clone(), tx).await??;
        self.wait_tx(txhash).await?;
        Ok(())
    }

    /// Converts ERG to MEL
    pub async fn erg_to_mel(&self, ergs: CoinValue) -> surf::Result<CoinValue> {
        let mut pool = self
            .client
            .latest_snapshot()
            .await?
            .get_pool(PoolKey::new(Denom::Mel, Denom::Erg))
            .await?
            .expect("no erg/mel pool");
        Ok(pool.swap_many(ergs.0, 0).1.into())
    }
}
