use std::{
    future::Future,
    net::SocketAddr,
    time::{Duration, Instant, SystemTime},
};

use crate::state::MintState;
use melwallet_client::WalletClient;
use smol::{
    channel::{Receiver, Sender},
    prelude::*,
};
use themelio_nodeprot::ValClient;
use themelio_stf::melpow;
use themelio_structs::{BlockHeight, CoinData, CoinValue, Denom, NetID, Transaction};

/// Worker configuration
#[derive(Clone, Debug)]
pub struct WorkerConfig {
    pub wallet: WalletClient,
    pub connect: SocketAddr,
    pub name: String,
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
    repeat_fallible(|| async {
        let mut my_speed = compute_speed().await;
        let client = get_valclient(
            opts.wallet.summary().await?.network == NetID::Testnet,
            opts.connect,
        )
        .await?;
        let snap = client.snapshot().await?;
        let max_speed = snap.current_header().dosc_speed as f64 / 30.0;

        let mint_state = MintState::new(opts.wallet.clone(), client.clone());

        loop {
            // turn off gracefully
            if recv_stop.try_recv().is_ok() {
                return Ok::<_, surf::Error>(());
            }

            // If we have more than 0.1 nomDOSC, convert it all to Mel.
            let our_doscs = opts
                .wallet
                .summary()
                .await?
                .detailed_balance
                .get("64")
                .copied()
                .unwrap_or_default();
            if our_doscs > CoinValue::from_millions(1u64) / 10 {
                log::info!("** [{}] CONVERTING {} ERG!", opts.name, our_doscs);
                mint_state.convert_doscs(our_doscs).await?;
            }

            log::info!("** [{}] My speed: {:.3} kH/s", opts.name, my_speed / 1000.0);
            log::info!(
                "** [{}]  Max speed: {:.3} kH/s",
                opts.name,
                max_speed / 1000.0
            );
            log::info!(
                "** [{}] Estimated return: {:.2} rDOSC/day",
                opts.name,
                my_speed * max_speed / max_speed.powi(2)
            );
            let my_difficulty = (my_speed * 3600.0).log2().ceil() as usize;
            let approx_iter = Duration::from_secs_f64(2.0f64.powi(my_difficulty as _) / my_speed);
            log::info!(
                "** [{}] Selected difficulty: {} (approx. {:?} / tx)",
                opts.name,
                my_difficulty,
                approx_iter
            );
            // repeat because wallet could be out of money
            let start = Instant::now();
            let (mut tx, earlier_height): (Transaction, BlockHeight) = repeat_fallible(|| async {
                let deadline = SystemTime::now()
                    + Duration::from_secs_f64(2.0f64.powi(my_difficulty as _) / my_speed);
                mint_state
                    .mint_transaction(my_difficulty)
                    .or(async move {
                        loop {
                            let now = SystemTime::now();
                            if let Ok(dur) = deadline.duration_since(now) {
                                log::debug!("approx {:?} left in iteration", dur);
                            }
                            smol::Timer::after(Duration::from_secs(60)).await;
                        }
                    })
                    .await
            })
            .await;
            let snap = repeat_fallible(|| client.snapshot()).await;
            let reward_speed = 2u128.pow(my_difficulty as u32)
                / (snap.current_header().height.0 + 5 - earlier_height.0) as u128;
            let reward = themelio_stf::calculate_reward(
                reward_speed,
                snap.current_header().dosc_speed,
                my_difficulty as u32,
            );
            let reward_nom = themelio_stf::dosc_to_erg(snap.current_header().height, reward);
            tx.outputs.push(CoinData {
                denom: Denom::Erg,
                value: reward_nom.into(),
                additional_data: vec![],
                covhash: opts.wallet.summary().await?.address,
            });
            my_speed = 2.0f64.powi(my_difficulty as _) / start.elapsed().as_secs_f64();
            log::info!(
                "** [{}] SUCCEEDED in minting a transaction producing {} ERG",
                opts.name,
                reward_nom,
            );
            mint_state.send_resigned_transaction(tx).await?;
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
