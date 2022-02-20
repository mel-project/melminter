use std::{
    future::Future,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use crate::state::MintState;
use dashmap::DashMap;
use melwallet_client::WalletClient;
use prodash::{messages::MessageLevel, unit::display::Mode};
use smol::channel::{Receiver, Sender};
use themelio_nodeprot::ValClient;
use themelio_stf::melpow;
use themelio_structs::{CoinDataHeight, CoinID, CoinValue, NetID};

/// Worker configuration
#[derive(Clone, Debug)]
pub struct WorkerConfig {
    pub wallet: WalletClient,
    pub connect: SocketAddr,
    pub name: String,
    pub tree: prodash::Tree,
    pub threads: usize,
}

/// Represents a worker.
pub struct Worker {
    send_stop: Sender<()>,
    _task: smol::Task<surf::Result<()>>,
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
    pub async fn wait(self) -> surf::Result<()> {
        self.send_stop.send(()).await?;
        self._task.await
    }
}

async fn main_async(opts: WorkerConfig, recv_stop: Receiver<()>) -> surf::Result<()> {
    let tree = opts.tree.clone();
    repeat_fallible(|| async {
        let worker = tree.add_child("worker");
        let worker = Arc::new(Mutex::new(worker));
        let my_speed = compute_speed().await;
        let is_testnet = opts.wallet.summary().await?.network == NetID::Testnet;
        let client = get_valclient(is_testnet, opts.connect).await?;

        let mint_state = MintState::new(opts.wallet.clone(), client.clone());

        loop {
            // turn off gracefully
            if recv_stop.try_recv().is_ok() {
                return Ok::<_, surf::Error>(());
            }

            // If we any erg, convert it all to mel.
            let our_doscs = opts
                .wallet
                .summary()
                .await?
                .detailed_balance
                .get("64")
                .copied()
                .unwrap_or_default();
            if our_doscs > CoinValue(0) {
                worker
                    .lock()
                    .unwrap()
                    .message(MessageLevel::Info, format!("CONVERTING {} ERG!", our_doscs));
                mint_state.convert_doscs(our_doscs).await?;
            }

            worker.lock().unwrap().message(
                MessageLevel::Info,
                format!("My speed: {:.3} kH/s", my_speed / 1000.0),
            );
            let my_difficulty = (my_speed * if is_testnet { 120.0 } else { 3600.0 })
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
            // repeat because wallet could be out of money
            let threads = opts.threads;
            let batch: Vec<(CoinID, CoinDataHeight, Vec<u8>)> = repeat_fallible(|| {
                let mint_state = &mint_state;
                let subworkers = DashMap::new();
                let worker = worker.clone();
                async move {
                    let total = 1usize << (my_difficulty.saturating_sub(10));
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
                    Ok::<_, surf::Error>(res)
                }
            })
            .await;
            worker.lock().unwrap().message(
                MessageLevel::Info,
                format!("built batch of {} future proofs", batch.len()),
            );
            let mut tasks = vec![];
            let worker = &worker;
            let mint_state = &mint_state;
            let client = &client;
            let exec = smol::Executor::new();
            for (coin, data, proof) in batch {
                tasks.push(exec.spawn(repeat_fallible(move || {
                    let data = data.clone();
                    let proof = proof.clone();
                    async move {
                        let mut sub = worker.lock().unwrap().add_child("submitting proof");
                        sub.init(None, None);
                        let snap = client.snapshot().await?;
                        let reward_speed = 2u128.pow(my_difficulty as u32)
                            / (snap.current_header().height.0 + 5 - data.height.0) as u128;
                        let reward = themelio_stf::calculate_reward(
                            reward_speed,
                            snap.current_header().dosc_speed,
                            my_difficulty as u32,
                        );
                        let reward_ergs =
                            themelio_stf::dosc_to_erg(snap.current_header().height, reward);
                        mint_state
                            .send_mint_transaction(
                                coin,
                                data.coin_data.clone(),
                                my_difficulty,
                                proof.clone(),
                                reward_ergs.into(),
                            )
                            .await?;
                        sub.message(
                            MessageLevel::Info,
                            format!("minted {} ERG", CoinValue(reward_ergs)),
                        );
                        Ok::<_, surf::Error>(())
                    }
                })));
            }
            exec.run(async {
                for task in tasks {
                    task.await;
                }
            })
            .await;
        }
    })
    .await;
    Ok(())
}

// Repeats something until it stops failing
async fn repeat_fallible<T, E: std::fmt::Debug, F: Future<Output = Result<T, E>>>(
    mut clos: impl FnMut() -> F,
) -> T {
    loop {
        match clos().await {
            Ok(val) => return val,
            Err(err) => log::debug!("retrying failed: {:?}", err),
        }
        smol::Timer::after(Duration::from_secs(1)).await;
    }
}

// Computes difficulty
async fn compute_speed() -> f64 {
    for difficulty in 1.. {
        let start = Instant::now();
        smol::unblock(move || melpow::Proof::generate(&[], difficulty)).await;
        let elapsed = start.elapsed();
        let speed = 2.0f64.powi(difficulty as _) / elapsed.as_secs_f64();
        if elapsed.as_secs_f64() > 2.0 {
            return speed;
        }
    }
    unreachable!()
}

async fn get_valclient(testnet: bool, connect: SocketAddr) -> anyhow::Result<ValClient> {
    let client = themelio_nodeprot::ValClient::new(
        if testnet {
            NetID::Testnet
        } else {
            NetID::Mainnet
        },
        connect,
    );
    if testnet {
        client.trust(themelio_bootstrap::checkpoint_height(NetID::Testnet).unwrap());
    } else {
        client.trust(themelio_bootstrap::checkpoint_height(NetID::Mainnet).unwrap());
    }
    Ok(client)
}
