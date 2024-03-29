use std::{future::Future, time::Duration};

use cmdopts::CmdOpts;

use melstructs::{CoinValue, Denom, NetID};

use prodash::{
    render::line::{self, StreamKind},
    Tree,
};

use state::MintState;
use structopt::StructOpt;

mod cmdopts;
mod state;
mod worker;
// use smol::prelude::*;
use crate::worker::{Worker, WorkerConfig};

fn main() -> anyhow::Result<()> {
    // let log_conf = std::env::var("RUST_LOG").unwrap_or_else(|_| "melminter=debug,warn".into());
    // std::env::set_var("RUST_LOG", log_conf);

    let dash_root = Tree::default();
    let dash_options = line::Options {
        keep_running_if_progress_is_empty: true,
        throughput: true,
        // hide_cursor: true,
        ..Default::default()
    }
    .auto_configure(StreamKind::Stdout);
    let _handle = line::render(std::io::stdout(), dash_root.clone(), dash_options);

    let opts: CmdOpts = CmdOpts::from_args();
    env_logger::init();
    smolscale::block_on(async move {
        let state = MintState::open(&opts.state, opts.network).await?;

        // background task to continually sync wallet

        // workers
        let mut workers = vec![];
        // make sure the worker has enough money
        // Move money if wallet does not have enough money
        while state
            .wallet
            .lock()
            .balances()
            .get(&Denom::Mel)
            .copied()
            .unwrap_or(CoinValue(0))
            < CoinValue::from_millions(1u64) / 20
        {
            // eprintln!("not enough money!");
            let _evt = dash_root
                .add_child("Melminter requires a small amount of 'seed' MEL to start minting.");
            let _evt = dash_root.add_child(format!(
                "Please send at least 0.1 MEL to {}",
                state.wallet.lock().address
            ));
            smol::Timer::after(Duration::from_secs(1)).await;
        }

        workers.push(Worker::start(WorkerConfig {
            state,
            payout: opts.payout,
            name: "".into(),
            tree: dash_root.clone(),
            threads: opts.threads.unwrap_or_else(num_cpus::get_physical),
            testnet: opts.network == NetID::Testnet,
        }));

        smol::future::pending().await
    })
}

// Repeats something until it stops failing
async fn repeat_fallible<T, E: std::fmt::Debug, F: Future<Output = Result<T, E>>>(
    mut clos: impl FnMut() -> F,
) -> T {
    loop {
        match clos().await {
            Ok(val) => return val,
            Err(err) => log::warn!("retrying failed: {:?}", err),
        }
        smol::Timer::after(Duration::from_secs(1)).await;
    }
}
