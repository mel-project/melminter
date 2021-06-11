use std::{
    future::Future,
    net::SocketAddr,
    time::{Duration, Instant, SystemTime},
};

use themelio_stf::{CoinData, Denom, Transaction, TxKind, melpow, melvm::Covenant};
use cmdopts::CmdOpts;
use nodeprot::ValClient;
use state::MintState;
use structopt::StructOpt;
mod cmdopts;
mod state;
use smol::prelude::*;

fn main() -> surf::Result<()> {
    let log_conf = std::env::var("RUST_LOG").unwrap_or_else(|_| "melminter=debug,warn".into());
    std::env::set_var("RUST_LOG", log_conf);
    let opts = CmdOpts::from_args();
    tracing_subscriber::fmt::init();
    smolscale::block_on(main_async(opts))
}

async fn main_async(opts: CmdOpts) -> surf::Result<()> {
    let mut my_speed = compute_speed().await;
    let client = get_valclient(opts.testnet, opts.connect).await?;
    let snap = client.snapshot().await?;
    let max_speed = snap.current_header().dosc_speed as f64 / 30.0;

    let mint_state = MintState::new(opts.wallet_url.clone(), opts.secret_key, client.clone());

    loop {
        log::info!("** My speed: {:.3} kH/s", my_speed / 1000.0);
        log::info!("** Max speed: {:.3} kH/s", max_speed / 1000.0);
        log::info!(
            "** Estimated return: {:.2} rDOSC/day",
            my_speed * max_speed / max_speed.powi(2)
        );
        let my_difficulty = (my_speed * 3100.0).log2().ceil() as usize;
        let approx_iter = Duration::from_secs_f64(2.0f64.powi(my_difficulty as _) / my_speed);
        log::info!(
            "** Selected difficulty: {} (approx. {:?} / tx)",
            my_difficulty,
            approx_iter
        );
        // repeat because wallet could be out of money
        let start = Instant::now();
        let (mut tx, earlier_height): (Transaction, u64) = repeat_fallible(|| async {
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
            / (snap.current_header().height + 5 - earlier_height) as u128;
        let reward = themelio_stf::calculate_reward(
            reward_speed,
            snap.current_header().dosc_speed,
            my_difficulty as u32,
        );
        let reward_nom = themelio_stf::dosc_inflate_r2n(snap.current_header().height, reward);
        tx.outputs.push(CoinData {
            denom: Denom::NomDosc,
            value: reward_nom,
            additional_data: vec![],
            covhash: tx.outputs[0].covhash,
        });
        tx.kind = TxKind::DoscMint;
        assert_eq!(
            tx.scripts[0],
            Covenant::std_ed25519_pk_new(opts.secret_key.to_public())
        );
        tx = tx
            .applied_fee(snap.current_header().fee_multiplier * 2, 1000, 0)
            .unwrap();
        tx.sigs.clear();
        for _ in 0..tx.inputs.len() {
            tx = tx.signed_ed25519(opts.secret_key)
        }
        my_speed = 2.0f64.powi(my_difficulty as _) / start.elapsed().as_secs_f64();
        dbg!(tx.fee);
        // panic!("oh no");
        mint_state.send_transaction(tx).await?;
    }
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
    let client = nodeprot::ValClient::new(
        if testnet {
            themelio_stf::NetID::Testnet
        } else {
            themelio_stf::NetID::Mainnet
        },
        connect,
    );
    if testnet {
        client.trust(
            2550,
            "2b2133e34779c4043278a5d084671a7a801022605dba2721e2d164d9c1096c13"
                .parse()
                .unwrap(),
        );
    } else {
        client.trust(
            14146,
            "50f5a41c6e996d36bc05b1272a59c8adb3fe3f98de70965abd2eed0c115d2108"
                .parse()
                .unwrap(),
        );
    }
    Ok(client)
}
