use std::{
    future::Future,
    process::{Command, Stdio},
    time::Duration,
};

use anyhow::Context;
use cmdopts::CmdOpts;

use melwallet_client::DaemonClient;
use prodash::{
    render::line::{self, StreamKind},
    Tree,
};
use structopt::StructOpt;
use tap::Tap;
use themelio_structs::{CoinValue, NetID};

mod cmdopts;
mod state;
mod worker;
// use smol::prelude::*;
use crate::worker::{Worker, WorkerConfig};

// get the version of this program itself (avoid use 'env!' because it will crashes if the program is compiled without cargo)
const VERSION: Option<&str> = option_env!("CARGO_PKG_VERSION");

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
        let daemon = DaemonClient::new(daemon_addr);
        let network_id = if opts.testnet {
            NetID::Testnet
        } else {
            NetID::Mainnet
        };

        println!("MelMinter v{} / melwalletd at {}", VERSION.unwrap_or("(N/A)"), daemon_addr);
        println!("");

        // workers
        let mut workers = vec![];
        let wallet_name = format!("{}{:?}", opts.wallet_prefix, network_id);
        // make sure the working-wallet exists
        let worker_wallet = match daemon.get_wallet(&wallet_name).await? {
            Some(wallet) => wallet,
            None => {
                let mut evt = dash_root.add_child(format!("creating new wallet {}", wallet_name));
                evt.init(None, None);
                log::info!("creating new wallet");
                daemon
                    .create_wallet(&wallet_name, opts.testnet, None, None)
                    .await?;
                daemon
                    .get_wallet(&wallet_name)
                    .await?
                    .context("just-created wallet failed?!")?
            }
        };
        worker_wallet.unlock(None).await?;

        // make sure the working-wallet has enough money (for paying fees)
        while worker_wallet
            .summary()
            .await?
            .detailed_balance
            .get("6d")
            .copied()
            .unwrap_or(CoinValue(0))
            < CoinValue::from_millions(1u64) / 20
        {
            let _evt = dash_root
                .add_child("The balance of melminter working wallet is less than 0.05! melminter requires a small amount of 'seed' MEL to start minting... (used by paying network fees)");
            let _evt = dash_root.add_child(format!(
                "Please send at least 0.1 MEL to {}",
                worker_wallet.summary().await?.address
            ));
            smol::Timer::after(Duration::from_secs(1)).await;

            if opts.skip_amount_check { break; }
        }

        workers.push(Worker::start(WorkerConfig {
            wallet: worker_wallet,
            payout: opts.payout,
            connect: themelio_bootstrap::bootstrap_routes(network_id)[0],
            name: "".into(),
            tree: dash_root.clone(),
            threads: opts.threads.unwrap_or_else(num_cpus::get_physical),
            diff: opts.fixed_diff,
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
