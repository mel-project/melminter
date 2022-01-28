use anyhow::Context;
use cmdopts::CmdOpts;

use melwallet_client::DaemonClient;
use structopt::StructOpt;
use themelio_structs::{CoinData, CoinID, CoinValue, Denom, TxKind};

mod cmdopts;
mod state;
mod worker;
// use smol::prelude::*;
use crate::worker::{Worker, WorkerConfig};

fn main() -> surf::Result<()> {
    let log_conf = std::env::var("RUST_LOG").unwrap_or_else(|_| "melminter=debug,warn".into());
    std::env::set_var("RUST_LOG", log_conf);
    let opts: CmdOpts = CmdOpts::from_args();
    tracing_subscriber::fmt::init();
    smolscale::block_on(async move {
        let daemon = DaemonClient::new(opts.daemon);
        let backup_wallet = daemon
            .get_wallet(&opts.backup_wallet)
            .await?
            .context("backup wallet does not exist")?;
        // workers
        let mut workers = vec![];
        for worker_id in 1..=num_cpus::get_physical() {
            log::info!("starting worker {}", worker_id);
            let wallet_name = format!("{}{}", opts.wallet_prefix, worker_id);
            // make sure the worker has enough money
            let worker_wallet = match daemon.get_wallet(&wallet_name).await? {
                Some(wallet) => wallet,
                None => {
                    log::info!("creating new wallet for worker {}", worker_id);
                    daemon
                        .create_wallet(&wallet_name, opts.testnet, None)
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
                log::warn!("worker {} does not have enough money, transferring money from the backup wallet!", worker_id);
                let tx = backup_wallet
                    .prepare_transaction(
                        TxKind::Normal,
                        vec![],
                        vec![CoinData {
                            covhash: worker_address,
                            value: CoinValue::from_millions(1u64) / 5,
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
                connect: opts.connect,
                name: format!("worker-{}", worker_id),
            }));
        }
        smol::future::pending().await
    })
}
