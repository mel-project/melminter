use std::net::SocketAddr;

use structopt::StructOpt;
use themelio_structs::Address;
// use tmelcrypt::Ed25519SK;

#[derive(Debug, StructOpt, Clone)]
pub struct CmdOpts {
    #[structopt(long)]
    /// Wallet API endpoint. For example localhost:11773
    pub daemon: Option<SocketAddr>,

    #[structopt(long, default_value = "__melminter_")]
    /// Prefixes for the "owned" wallets created by the melminter.
    pub wallet_prefix: String,

    #[structopt(long)]
    /// Payout address for melminter profits.
    pub payout: Address,

    #[structopt(long)]
    /// Whether to use testnet
    pub testnet: bool,

    #[structopt(long)]
    /// Force a certain number of threads. Defaults to the number of *physical* CPUs.
    pub threads: Option<usize>,
}
