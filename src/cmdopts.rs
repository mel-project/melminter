use std::path::PathBuf;

use melstructs::{Address, NetID};
use structopt::StructOpt;
// use tmelcrypt::Ed25519SK;

#[derive(Debug, StructOpt, Clone)]
pub struct CmdOpts {
    #[structopt(long, default_value = ".melminter/")]
    /// Path to store state. Defaults to .melminter in the *current* directory.
    pub state: PathBuf,

    #[structopt(long)]
    /// Payout address for melminter profits.
    pub payout: Address,

    #[structopt(long, default_value = "mainnet")]
    /// Whether to use testnet
    pub network: NetID,

    #[structopt(long)]
    /// Force a certain number of threads. Defaults to the number of *physical* CPUs.
    pub threads: Option<usize>,
    // #[structopt(long)]
    // /// Drain the fee reserve at the start.
    // pub drain_reserve: bool,
}
