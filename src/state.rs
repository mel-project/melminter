use std::{collections::BTreeMap, path::Path, sync::Arc, time::Duration};

use anyhow::Context;
use bytes::Bytes;
use melprot::{Client, CoinChange};
use melstf::Tip910MelPowHash;
use melstructs::{
    Address, BlockHeight, CoinData, CoinDataHeight, CoinID, CoinValue, Denom, NetID, PoolKey,
    Transaction, TxHash, TxKind,
};

use melwallet::{PrepareTxArgs, StdEd25519Signer, Wallet};
use parking_lot::Mutex;

use smol::Task;
use stdcode::StdcodeSerializeExt;
use tmelcrypt::Ed25519SK;

use crate::repeat_fallible;

#[derive(Clone)]
pub struct MintState {
    pub wallet: Arc<Mutex<Wallet>>,
    pub sk: Ed25519SK,
    pub client: Client,
    _wallet_sync_task: Arc<Task<()>>,
}

async fn wallet_sync_loop(wallet: Arc<Mutex<Wallet>>, client: Client) -> anyhow::Result<()> {
    let wallet_addr = wallet.lock().address;
    // sync new blocks in a loop
    loop {
        let latest_height = client.latest_snapshot().await?.current_header().height;
        let wallet_height = wallet.lock().height;
        for height in (wallet_height.0 + 1)..(latest_height.0 + 1) {
            let snapshot = client.snapshot(BlockHeight(height)).await?;
            let ccs = snapshot.get_coin_changes(wallet_addr).await?;
            log::debug!("syncing at height {height} with coin changes {:?}!", ccs);
            let mut new_coins = vec![];
            let mut spent_coins = vec![];
            for cc in ccs {
                match cc {
                    CoinChange::Add(id) => {
                        if let Some(data_height) = snapshot.get_coin(id).await? {
                            new_coins.push((id, data_height.coin_data));
                        }
                    }
                    CoinChange::Delete(id, _) => {
                        spent_coins.push(id);
                    }
                }
            }
            wallet
                .lock()
                .add_coins(snapshot.current_header().height, new_coins, spent_coins)?;
        }
        // if our wallet still has pending transactions new blocks have been produced, retransmit
        if !wallet.lock().pending_outgoing.is_empty() && latest_height > wallet_height {
            let pending_outgoing = wallet.lock().pending_outgoing.clone();
            for (_, tx) in pending_outgoing.into_iter() {
                client
                    .latest_snapshot()
                    .await?
                    .get_raw()
                    .send_tx(tx)
                    .await??;
            }
        }

        smol::Timer::after(Duration::from_secs(10)).await;
    }
}

async fn open_wallet(client: &Client, secret: Ed25519SK) -> anyhow::Result<Wallet> {
    // resync everything in the beginning
    let latest_snapshot = client.latest_snapshot().await?;
    let cov = melvm::Covenant::std_ed25519_pk_new(secret.to_public());
    let address = cov.hash();
    let mut wallet = Wallet {
        address,
        height: BlockHeight(0),
        confirmed_utxos: BTreeMap::new(),
        pending_outgoing: BTreeMap::new(),
        netid: client.netid(),
    };
    let owned_coins = latest_snapshot
        .get_coins(address)
        .await?
        .context("server does not support coin index, but we need it")?;
    log::debug!("initially loading wallet with coins {:?}!", owned_coins);
    wallet.full_reset(latest_snapshot.current_header().height, owned_coins)?;
    Ok(wallet)
}

impl MintState {
    /// Open a new MintState, given a folder where all persistent state is stored.
    pub async fn open(state_folder: &Path, network: NetID) -> anyhow::Result<Self> {
        std::fs::create_dir_all(state_folder)?;
        let sk = {
            let mut sk_path = state_folder.to_owned();
            sk_path.push("secret");
            match std::fs::read(&sk_path) {
                Ok(bts) => stdcode::deserialize(&bts)?,
                Err(_) => {
                    // create a new secret key and store it
                    let key = Ed25519SK::generate();
                    std::fs::write(&sk_path, key.stdcode())?;
                    key
                }
            }
        };
        let client = Client::autoconnect(network).await?;
        let wallet = open_wallet(&client, sk).await?;
        let wallet = Arc::new(Mutex::new(wallet));
        Ok(Self {
            wallet: wallet.clone(),
            sk,
            client: client.clone(),
            _wallet_sync_task: Arc::new(smolscale::spawn(repeat_fallible(move || {
                wallet_sync_loop(wallet.clone(), client.clone())
            }))),
        })
    }

    // helpers
    pub async fn prepare_tx(&self, args: PrepareTxArgs) -> anyhow::Result<Transaction> {
        let signer = StdEd25519Signer(self.sk);
        let fee_multiplier = self
            .client
            .latest_snapshot()
            .await?
            .current_header()
            .fee_multiplier;
        let tx = self
            .wallet
            .lock()
            .prepare_tx(args, &signer, fee_multiplier)?;
        Ok(tx)
    }

    pub async fn send_raw(&self, tx: Transaction) -> anyhow::Result<()> {
        self.client
            .latest_snapshot()
            .await?
            .get_raw()
            .send_tx(tx.clone())
            .await??;
        self.wallet.lock().add_pending(tx);

        Ok(())
    }

    pub async fn wait_tx(&self, txhash: TxHash) -> anyhow::Result<()> {
        while self.wallet.lock().pending_outgoing.get(&txhash).is_some() {
            smol::Timer::after(Duration::from_secs(1)).await;
        }
        Ok(())
    }

    /// Generates a list of "seed" coins.
    ///
    /// The minter needs to prove that it performed some computation between some time `t` and now (Something like `H(H(H(x)))`).
    /// where `x` is only known to it after a certain amount of time `t` has passed.
    ///
    /// To ensure that `x` is unique and NOT reusable to produce multiple proofs, so we
    /// calculate it with the following:
    /// 1. The spending of these "seed" coins: The coins can only be spent
    /// once.
    /// 2. The block hash of the block containining the transaction that produced the first coin
    ///    being spent - this is a value that isn't known until that block is produced.
    pub async fn generate_seeds(&self, threads: usize) -> anyhow::Result<()> {
        let my_address = self.wallet.lock().address;
        loop {
            let toret = self.get_seeds_raw().await?;
            if toret.len() >= threads {
                return Ok(());
            }
            // generate a bunch of custom-token utxos
            let tx = self
                .prepare_tx(PrepareTxArgs {
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
                    data: bytes::Bytes::new(),
                    fee_ballast: 0,
                })
                .await?;
            self.send_raw(tx.clone()).await?;
            self.wait_tx(tx.hash_nosigs()).await?;
        }
    }

    async fn get_seeds_raw(&self) -> anyhow::Result<Vec<CoinID>> {
        Ok(self
            .wallet
            .lock()
            .confirmed_utxos
            .iter()
            .filter_map(|(k, v)| {
                if matches!(v.coin_data.denom, Denom::Custom(_)) {
                    Some((k, v))
                } else {
                    None
                }
            })
            .map(|d| *d.0)
            .collect())
    }

    /// Creates a partially-filled-in transaction, with the given difficulty, that's neither signed nor fee'd. The caller should fill in the DOSC output.
    pub async fn mint_batch(
        &self,
        difficulty: usize,
        on_progress: impl Fn(usize, f64) + Sync + Send + 'static,
        threads: usize,
    ) -> anyhow::Result<Vec<(CoinID, CoinDataHeight, Vec<u8>)>> {
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
            let chi = tmelcrypt::hash_keyed(tip_header_hash, &seed.stdcode());
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

    async fn address(&self) -> anyhow::Result<Address> {
        Ok(self.wallet.lock().address)
    }

    /// Sends a transaction.
    pub async fn send_mint_transaction(
        &self,
        seed: CoinID,
        difficulty: usize,
        proof: Vec<u8>,
        ergs: CoinValue,
    ) -> anyhow::Result<TxHash> {
        let own_cov = self.wallet.lock().address;
        let height = self.wallet.lock().height;
        let seed_cdh = self
            .client
            .snapshot(height)
            .await?
            .get_coin(seed)
            .await?
            .unwrap();
        let tx = self
            .prepare_tx(PrepareTxArgs {
                kind: TxKind::DoscMint,
                inputs: vec![(seed, seed_cdh)],
                outputs: vec![CoinData {
                    denom: Denom::Erg,
                    value: ergs,
                    additional_data: vec![].into(),
                    covhash: own_cov,
                }],
                covenants: vec![],
                data: Bytes::copy_from_slice(&(difficulty, proof).stdcode()),
                fee_ballast: 0,
            })
            .await?;
        self.send_raw(tx.clone()).await?;
        Ok(tx.hash_nosigs())
    }

    /// Converts a given number of doscs to mel.
    pub async fn convert_doscs(&self, doscs: CoinValue) -> anyhow::Result<()> {
        let my_address = self.address().await?;
        let tx = self
            .prepare_tx(PrepareTxArgs {
                kind: TxKind::Swap,
                inputs: vec![],
                outputs: vec![CoinData {
                    covhash: my_address,
                    value: doscs,
                    denom: Denom::Erg,
                    additional_data: vec![].into(),
                }],
                covenants: vec![],
                data: PoolKey::new(Denom::Mel, Denom::Erg).to_bytes(),
                fee_ballast: 0,
            })
            .await?;

        self.send_raw(tx.clone()).await?;
        self.wait_tx(tx.hash_nosigs()).await?;
        Ok(())
    }

    /// removes any 0 value erg coins which prevent minting transactions
    pub async fn clean_ergs(
        &self,
        empty_ergs: Vec<(CoinID, CoinDataHeight)>,
    ) -> anyhow::Result<()> {
        let tx = self
            .prepare_tx(PrepareTxArgs {
                kind: TxKind::Normal,
                inputs: empty_ergs,
                outputs: vec![CoinData {
                    covhash: Address::coin_destroy(),
                    value: CoinValue(0),
                    denom: Denom::Erg,
                    additional_data: vec![].into(),
                }],
                covenants: vec![],
                data: Bytes::new(),
                fee_ballast: 0,
            })
            .await?;
        self.send_raw(tx.clone()).await?;
        self.wait_tx(tx.hash_nosigs()).await?;

        Ok(())
    }
}
