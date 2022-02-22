use anyhow::Context;
use cmdopts::CmdOpts;

use melwallet_client::DaemonClient;
use prodash::{
    render::line::{self, StreamKind},
    Tree, Unit,
};
use structopt::StructOpt;
use themelio_structs::{CoinData, CoinID, CoinValue, Denom, NetID, TxKind};

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
    let handle = line::render(std::io::stdout(), dash_root.clone(), dash_options);

    let opts: CmdOpts = CmdOpts::from_args();
    env_logger::init();
    smolscale::block_on(async move {
        let daemon = DaemonClient::new(opts.daemon);
        let backup_wallet = daemon
            .get_wallet(&opts.backup_wallet)
            .await?
            .context("backup wallet does not exist")?;
        let network_id = backup_wallet.summary().await?.network;
        // workers
        let mut workers = vec![];
        let wallet_name = format!("{}{:?}", opts.wallet_prefix, network_id);
        // make sure the worker has enough money
        let worker_wallet = match daemon.get_wallet(&wallet_name).await? {
            Some(wallet) => wallet,
            None => {
                let mut evt = dash_root.add_child(format!("creating new wallet {}", wallet_name));
                evt.init(None, None);
                log::info!("creating new wallet");
                daemon
                    .create_wallet(
                        dbg!(&wallet_name),
                        backup_wallet.summary().await?.network == NetID::Testnet,
                        None,
                    )
                    .await?;
                daemon
                    .get_wallet(&wallet_name)
                    .await?
                    .context("just-created wallet failed?!")?
            }
        };
        worker_wallet.unlock(None).await?;
        let worker_address = worker_wallet.summary().await?.address;

        // Move money if wallet does not have enough money
        if worker_wallet
            .summary()
            .await?
            .detailed_balance
            .get("6d")
            .copied()
            .unwrap_or(CoinValue(0))
            < CoinValue::from_millions(1u64) / 10
        {
            let mut evt = dash_root.add_child("moving money from the backup wallet");
            evt.init(None, Some(Unit::from("")));
            let tx = backup_wallet
                .prepare_transaction(
                    TxKind::Normal,
                    vec![],
                    vec![CoinData {
                        covhash: worker_address,
                        value: CoinValue::from_millions(1u64) / 10,
                        denom: Denom::Mel,
                        additional_data: vec![],
                    }],
                    vec![],
                    vec![],
                    vec![],
                )
                .await?;
            let txhash = backup_wallet.send_tx(tx).await?;
            log::warn!("waiting for txhash {:?}...", txhash);
            backup_wallet.wait_transaction(txhash).await?;
            worker_wallet.add_coin(CoinID { txhash, index: 0 }).await?;
        }

        workers.push(Worker::start(WorkerConfig {
            wallet: worker_wallet,
            backup: backup_wallet.clone(),
            connect: themelio_bootstrap::bootstrap_routes(backup_wallet.summary().await?.network)
                [0],
            name: "".into(),
            tree: dash_root.clone(),
            threads: opts.threads.unwrap_or_else(num_cpus::get_physical),
        }));

        smol::future::pending().await
    })
}
