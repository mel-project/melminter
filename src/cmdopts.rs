use std::net::SocketAddr;

use structopt::StructOpt;
use tmelcrypt::Ed25519SK;
#[derive(Debug, StructOpt, Clone)]
pub struct CmdOpts {
    #[structopt(long)]
    /// The unlocking secret key, in hex.
    pub secret_key: Ed25519SK,

    #[structopt(long)]
    /// Wallet API endpoint. For example http://localhost:12345/wallets/test
    pub wallet_url: String,

    #[structopt(long)]
    /// Is this a testnet wallet
    pub testnet: bool,

    #[structopt(long)]
    /// Where to connect
    pub connect: SocketAddr,
}
