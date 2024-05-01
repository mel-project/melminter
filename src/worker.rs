use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use crate::{repeat_fallible, state::MintState};
use bytes::Bytes;
use dashmap::{mapref::multiple::RefMulti, DashMap};
use melstf::Tip910MelPowHash;
use melstructs::{
    Address, CoinData, CoinDataHeight, CoinID, CoinValue, Denom, PoolKey, TxKind,
};

use melwallet::PrepareTxArgs;
use prodash::{messages::MessageLevel, tree::Item, unit::display::Mode};
use smol::{
    channel::{Receiver, Sender},
    Task,
};

/// Worker configuration
#[derive(Clone)]
pub struct WorkerConfig {
    pub state: MintState,
    pub payout: Address,
    pub name: String,
    pub tree: prodash::Tree,
    pub threads: usize,
    pub testnet: bool,
}

/// Represents a worker.
pub struct Worker {
    send_stop: Sender<()>,
    _task: smol::Task<anyhow::Result<()>>,
}

impl Worker {
    /// Starts a worker with the given WorkerConfig.
    pub fn start(config: WorkerConfig) -> Self {
        let (send_stop, recv_stop) = smol::channel::bounded(1);
        Self {
            send_stop,
            _task: smolscale::spawn(main_async(config, recv_stop)),
        }
    }

    /// Waits for the worker to complete the current iteration, then stops it.
    pub async fn wait(self) -> anyhow::Result<()> {
        self.send_stop.send(()).await?;
        self._task.await
    }
}

async fn main_async(opts: WorkerConfig, recv_stop: Receiver<()>) -> anyhow::Result<()> {
    let tree = opts.tree.clone();
    repeat_fallible(|| {
        let tree = tree.clone();
        let recv_stop = recv_stop.clone();
        let opts = opts.clone();
        async move {
        let worker = tree.add_child("worker");
        let worker = Arc::new(Mutex::new(worker));
        let my_speed = compute_speed().await;
        let is_testnet = opts.testnet;

        let mint_state = opts.state;
        loop {
            // turn off gracefully
            if recv_stop.try_recv().is_ok() {
                return Ok::<_, anyhow::Error>(());
            }

            let snapshot = mint_state.client.latest_snapshot().await?;
            let erg_to_mel = snapshot
                .get_pool(PoolKey::new(Denom::Mel, Denom::Erg))
                .await?
                .expect("must have erg-mel pool");

            // If we have any erg, convert it all to mel.
            let balances = mint_state.wallet.lock().balances();
            let our_ergs = balances.get(&Denom::Erg);
            if let Some(ergs) = our_ergs {
                worker
                    .lock()
                    .unwrap()
                    .message(MessageLevel::Info, format!("CONVERTING {} ERG!", ergs));
                mint_state.clean_ergs().await?;
                mint_state.convert_doscs(*ergs).await?;
            }

            // If we have more than 1 MEL, transfer half to the backup wallet.
            let our_mels = *mint_state.wallet.lock().balances().get(&Denom::Mel).unwrap_or(&CoinValue(0));
            if our_mels > CoinValue::from_millions(1u8) {
                let to_convert = our_mels / 2;
                worker.lock().unwrap().info(format!(
                    "transferring {} MEL of profits to backup wallet",
                    to_convert
                ));
                let tx = mint_state.prepare_tx(PrepareTxArgs{ 
                    kind: TxKind::Normal, inputs: vec![], outputs: vec![CoinData {
                    covhash: opts.payout,
                    value: to_convert,
                    additional_data: vec![].into(),
                    denom: Denom::Mel,
                }], covenants: vec![], data: Bytes::new(), fee_ballast: 0 }).await?;

                mint_state.send_raw(tx.clone()).await?;
                mint_state.wait_tx(tx.hash_nosigs()).await?;
            }

            let my_difficulty = (my_speed * if is_testnet { 120.0 } else { 30000.0 })
                .log2()
                .ceil() as usize;
            let approx_iter = Duration::from_secs_f64(2.0f64.powi(my_difficulty as _) / my_speed);
            worker.lock().unwrap().message(
                MessageLevel::Info,
                format!(
                    "Selected difficulty: {} (approx. {:?} / tx)",
                    my_difficulty, approx_iter
                ),
            );
            let threads = opts.threads;
            let fastest_speed = mint_state.client.latest_snapshot().await?.current_header().dosc_speed as f64 / 30.0;
            worker.lock().unwrap().info(format!(
                "Max speed on chain: {:.2} kH/s",
                fastest_speed / 1000.0
            ));

            // generates some seeds
            {
                let mut sub = worker
                    .lock()
                    .unwrap()
                    .add_child("generating seed UTXOs for minting...");
                sub.init(None, None);
                mint_state.generate_seeds(threads).await?;
            }
            let batch: Vec<(CoinID, CoinDataHeight, Vec<u8>)> = repeat_fallible(|| {
                let mint_state = mint_state.clone();
                let subworkers = Arc::new(DashMap::new());
                let worker = worker.clone();

                // background task that tallies speeds
                let speed_task: Arc<Task<()>> = {
                    let subworkers = subworkers.clone();
                    let worker = worker.clone();
                    let snapshot = snapshot.clone();
                    let mint_state = mint_state.clone();
                    Arc::new(smolscale::spawn(async move {
                        let mut previous: HashMap<usize, usize> = HashMap::new();
                        let mut _space = None;
                        let mut delta_sum = 0;
                        let start = Instant::now();
                        loop {
                            smol::Timer::after(Duration::from_secs(1)).await;
                            subworkers.iter().for_each(|pp: RefMulti<usize, Item>| {
                                let prev = previous.entry(*pp.key()).or_insert(0usize);
                                let curr = pp.value().step().unwrap_or_default();
                                delta_sum += curr.saturating_sub(*prev);
                                *prev = curr;
                            });
                            let speed = (delta_sum * 1024) as f64 / start.elapsed().as_secs_f64();
                            let per_core_speed = speed / (threads as f64);
                            let dosc_per_day =
                                (per_core_speed / fastest_speed).powi(2) * (threads as f64);
                            let erg_per_day = dosc_per_day
                                * (melstf::dosc_to_erg(
                                    snapshot.current_header().height,
                                    10000,
                                ) as f64)
                                / 10000.0;
                            let (_, mel_per_day) = erg_to_mel
                                .clone()
                                .swap_many((erg_per_day * 10000.0) as u128, 0);
                            let mel_per_day = mel_per_day as f64 / 10000.0;
                            let balance = *mint_state.wallet.lock().balances().get(&Denom::Mel).unwrap();
                            let mut new = worker.lock().unwrap().add_child(format!(
                                "daily return: {:.3} DOSC ≈ {:.3} ERG ≈ {:.3} MEL; fee reserve {} MEL",
                                dosc_per_day, erg_per_day, mel_per_day, balance
                            ));
                            new.init(None, None);
                            _space = Some(new)
                        }
                    }))
                };
                let mint_state = mint_state;
                async move {
                    let total = 100 * (1usize << (my_difficulty.saturating_sub(10)));
                    let res = mint_state
                        .mint_batch(
                            my_difficulty,
                            move |a, b| {
                                let mut subworker = subworkers.entry(a).or_insert_with(|| {
                                    let mut child = worker
                                        .lock()
                                        .unwrap()
                                        .add_child(format!("subworker {}", a));
                                    child.init(
                                        Some(total),
                                        Some(prodash::unit::dynamic_and_mode(
                                            "kH",
                                            Mode::with_throughput(),
                                        )),
                                    );
                                    child
                                });
                                subworker.set(((total as f64) * b) as usize);
                            },
                            threads,
                        )
                        .await?;
                    drop(speed_task);
                    Ok::<_, anyhow::Error>(res)
                }
            })
            .await;
            worker.lock().unwrap().message(
                MessageLevel::Info,
                format!("built batch of {} future proofs", batch.len()),
            );

            // Time to submit the proofs. For every proof in the batch, we attempt to submit it. If the submission fails, we move on, because there might be some weird race condition with melwalletd going on.
            // We also attempt to submit transactions in parallel. This is done by retrying *only* on insufficient funds.
            let mut to_wait = vec![];
            {
                let mut sub = worker.lock().unwrap().add_child("submitting proof");
                sub.init(Some(batch.len()), None);
                for (coin, data, proof) in batch {
                    sub.inc();
                    loop {
                        let reward_attempt = async {
                            // Retry until we don't see insufficient funds
                            let reward_ergs = loop {
                                let snap = mint_state.client.latest_snapshot().await?;
                                let reward_speed = 2u128.pow(my_difficulty as u32)
                                    / (snap.current_header().height.0 + 40 - data.height.0) as u128;
                                let reward = melstf::calculate_reward(
                                    reward_speed * 100,
                                    snap.current_header().dosc_speed,
                                    my_difficulty as u32,
                                    true
                                );
                                let reward_ergs =
                                    melstf::dosc_to_erg(snap.current_header().height, reward);
                                match mint_state
                                    .send_mint_transaction(
                                        coin,
                                        my_difficulty,
                                        proof.clone(),
                                        reward_ergs.into(),
                                    )
                                    .await
                                {
                                    Err(err) => {
                                        if err.to_string().contains("preparation") || err.to_string().contains("timeout") {
                                            let mut sub = sub.add_child("waiting for available coins ".to_owned() + &err.to_string());
                                            sub.init(None, None);
                                            smol::Timer::after(Duration::from_secs(10)).await;
                                        } else {
                                            log::error!("error sending mint transaction: {err}");
                                            anyhow::bail!(err)
                                        }
                                        }
                                    Ok(res) => {
                                        to_wait.push(res);
                                        break reward_ergs;
                                    }
                                }
                            };
                            sub.info(format!("minted {} ERG", CoinValue(reward_ergs)));
                            Ok::<_, anyhow::Error>(())
                        }
                        .await;
                        if let Err(err) = reward_attempt {
                            sub.info(format!(
                                "FAILED a proof submission for some reason : {:?}",
                                err
                            ));
                        } else {
                            break
                        }
                    }
                }
            }
            let mut sub = worker
                .lock()
                .unwrap()
                .add_child("waiting for confirmation of proof");
            sub.init(Some(to_wait.len()), None);
            for to_wait in to_wait {
                sub.inc();
                mint_state.wait_tx(to_wait).await?;
            }
        }
    }})
    .await;
    Ok(())
}

// Computes difficulty
async fn compute_speed() -> f64 {
    for difficulty in 1.. {
        let start = Instant::now();
        smol::unblock(move || melpow::Proof::generate(&[], difficulty, Tip910MelPowHash)).await;
        let elapsed = start.elapsed();
        let speed = 2.0f64.powi(difficulty as _) / elapsed.as_secs_f64();
        if elapsed.as_secs_f64() > 0.5 {
            return speed;
        }
    }
    unreachable!()
}
