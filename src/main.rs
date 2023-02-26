use std::{
    future::Future,
    process::{Command, Stdio},
    sync::Arc,
    time::Duration,
};

use anyhow::Context;
use cmdopts::CmdOpts;

use melstructs::{CoinValue, NetID};
use melwallet_client::DaemonClient;
use melwalletd_prot::MelwalletdClient;
use prodash::{
    render::line::{self, StreamKind},
    Tree,
};
use structopt::StructOpt;
use tap::Tap;

mod cmdopts;
mod state;
mod worker;
// use smol::prelude::*;
use crate::worker::{Worker, WorkerConfig};

fn main() -> surf::Result<()> {
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
    smol::block_on(async move {
        // either start a daemon, or use the provided one
        let mut _running_daemon = None;
        let daemon_addr = if let Some(addr) = opts.daemon {
            addr
        } else {
            // start a daemon naw
            let port = fastrand::usize(5000..15000);
            let daemon = Command::new("melwalletd")
                .arg("--listen")
                .arg(format!("127.0.0.1:{}", port))
                .arg("--wallet-dir")
                .arg(dirs::config_dir().unwrap().tap_mut(|p| p.push("melminter")))
                .stderr(Stdio::null())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .spawn()
                .unwrap();
            smol::Timer::after(Duration::from_secs(1)).await;
            _running_daemon = Some(daemon);
            format!("127.0.0.1:{}", port).parse().unwrap()
        };
        scopeguard::defer!({
            if let Some(mut d) = _running_daemon {
                let _ = d.kill();
            }
        });
        let daemon = MelwalletdClient::from(DaemonClient::new(daemon_addr));
        let network_id = if opts.testnet {
            NetID::Testnet
        } else {
            NetID::Mainnet
        };
        // workers
        let mut workers = vec![];
        let wallet_name = format!("{}{:?}", opts.wallet_prefix, network_id);
        // make sure the worker has enough money
        let daemon = match daemon.wallet_summary(wallet_name.clone()).await? {
            Ok(wallet) => daemon,
            Err(_) => {
                let mut evt = dash_root.add_child(format!("creating new wallet {}", wallet_name));
                evt.init(None, None);
                log::info!("creating new wallet");
                daemon
                    .create_wallet(wallet_name.clone(), "".into(), None)
                    .await??;
                daemon
            }
        };
        daemon
            .unlock_wallet(wallet_name.clone(), "".into())
            .await??;

        // Move money if wallet does not have enough money
        while daemon
            .wallet_summary(wallet_name.clone())
            .await??
            .detailed_balance
            .get("MEL")
            .copied()
            .unwrap_or(CoinValue(0))
            < CoinValue::from_millions(1u64) / 20
        {
            let _evt = dash_root
                .add_child("Melminter requires a small amount of 'seed' MEL to start minting.");
            let _evt = dash_root.add_child(format!(
                "Please send at least 0.1 MEL to {}",
                daemon.wallet_summary(wallet_name.clone()).await??.address
            ));
            smol::Timer::after(Duration::from_secs(1)).await;
        }

        workers.push(Worker::start(WorkerConfig {
            daemon: Arc::new(daemon),
            payout: opts.payout,
            connect: melbootstrap::bootstrap_routes(network_id)[0],
            name: "".into(),
            tree: dash_root.clone(),
            threads: opts.threads.unwrap_or_else(num_cpus::get_physical),
            testnet: opts.testnet,
            wallet_name: wallet_name.clone(),
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
