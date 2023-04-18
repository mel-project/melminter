use std::{
    collections::BTreeMap,
    fs::File,
    future::Future,
    io::{Read, Write},
    net::SocketAddr,
    time::Duration,
};

use cmdopts::CmdOpts;

use melprot::Client;
use melstructs::{BlockHeight, CoinValue, Denom, NetID};
use melwallet::Wallet;
use prodash::{
    render::line::{self, StreamKind},
    Tree,
};
use serde::{Deserialize, Serialize};
use state::MintState;
use structopt::StructOpt;
use tmelcrypt::Ed25519SK;

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
        let network_id = if opts.testnet {
            NetID::Testnet
        } else {
            NetID::Mainnet
        };
        let wallet_name = format!("{}{:?}", opts.wallet_prefix, network_id);
        let (wallet, sk) = import_or_default(&wallet_name, network_id).await?;
        let client = get_client(network_id, melbootstrap::bootstrap_routes(network_id)[0]).await?;
        let state = MintState::new(wallet, sk, client);

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
            testnet: opts.testnet,
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

#[derive(Serialize, Deserialize)]
struct WalletSecret(Ed25519SK);

async fn import_or_default(
    wallet_name: &str,
    network_id: NetID,
) -> anyhow::Result<(Wallet, Ed25519SK)> {
    // attempt to retrieve the secret key stored at `wallet_name`.txt
    let sk_file = wallet_name.to_owned() + ".txt";

    let secret: Ed25519SK = if let Ok(mut sk_f) = File::open(sk_file.clone()) {
        let mut sk_str = String::new();
        sk_f.read_to_string(&mut sk_str)?;
        println!("wallet_name content: {sk_str}");
        let sk = serde_json::from_str(&sk_str)?;
        sk
    } else {
        let sk = Ed25519SK::generate();
        let mut sk_f = File::create(sk_file)?;
        let sk_str = serde_json::to_string(&sk)?;
        sk_f.write(sk_str.as_bytes())?;
        sk
    };
    // create new sk & store it
    // make wallet
    let cov = melvm::Covenant::std_ed25519_pk_new(secret.to_public());
    let addr = cov.hash();
    let wallet = Wallet {
        address: addr,
        height: BlockHeight(0),
        confirmed_utxos: BTreeMap::new(),
        pending_outgoing: BTreeMap::new(),
        netid: network_id,
    };
    Ok((wallet, secret))
}

async fn get_client(network_id: NetID, connect: SocketAddr) -> anyhow::Result<Client> {
    let client = Client::connect_http(network_id, connect).await?;
    match network_id {
        NetID::Testnet => client.trust(melbootstrap::checkpoint_height(NetID::Testnet).unwrap()),
        NetID::Mainnet => client.trust(melbootstrap::checkpoint_height(NetID::Mainnet).unwrap()),
        _ => anyhow::bail!("melminter only supported for testnet and mainnet"),
    }
    Ok(client)
}
