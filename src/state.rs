use anyhow::Context;
use blkstructs::{melvm::Covenant, CoinData, Denom, Transaction, MICRO_CONVERTER};
use nodeprot::ValClient;
use serde::{Deserialize, Serialize};
use tmelcrypt::Ed25519SK;

#[derive(Debug, Clone)]
pub struct MintState {
    wallet_url: String,
    my_sk: Ed25519SK,
    client: ValClient,
}

#[derive(Serialize, Deserialize)]
struct PrepareReq {
    signing_key: String,
    outputs: Vec<CoinData>,
}

impl MintState {
    pub fn new(wallet_url: String, my_sk: Ed25519SK, client: ValClient) -> Self {
        Self {
            wallet_url,
            my_sk,
            client,
        }
    }

    async fn prepare_dummy(&self) -> surf::Result<Transaction> {
        let req = PrepareReq {
            signing_key: hex::encode(&self.my_sk.0),
            outputs: vec![CoinData {
                covhash: Covenant::std_ed25519_pk_new(self.my_sk.to_public()).hash(),
                denom: Denom::Mel,
                value: MICRO_CONVERTER,
                additional_data: vec![],
            }],
        };
        let mut res = surf::post(format!("{}/prepare-tx", self.wallet_url))
            .body(serde_json::to_vec(&req).unwrap())
            .await?;
        Ok(res.body_json().await?)
    }

    /// Creates a partially-filled-in transaction, with the given difficulty, that's neither signed nor feed. The caller should fill in the DOSC output.
    pub async fn mint_transaction(&self, difficulty: usize) -> surf::Result<(Transaction, u64)> {
        let mut transaction = self.prepare_dummy().await?;
        let tip_cdh = self
            .client
            .snapshot()
            .await?
            .get_coin(transaction.inputs[0])
            .await?
            .context("dummy transaction's input spent from behind our back")?;
        let tip_header_hash = self
            .client
            .snapshot()
            .await?
            .get_history(tip_cdh.height)
            .await?
            .expect("history not found")
            .hash();
        let chi = tmelcrypt::hash_keyed(
            &tip_header_hash,
            &stdcode::serialize(&transaction.inputs[0]).unwrap(),
        );
        let proof = smol::unblock(move || melpow::Proof::generate(&chi, difficulty)).await;
        let difficulty = difficulty as u32;
        let proof_bytes = proof.to_bytes();
        assert!(melpow::Proof::from_bytes(&proof_bytes)
            .unwrap()
            .verify(&chi, difficulty as usize));

        transaction.data = stdcode::serialize(&(difficulty, proof_bytes)).unwrap();

        Ok((transaction, tip_cdh.height))
    }

    /// Sends a transaction out.
    pub async fn send_transaction(&self, transaction: Transaction) -> surf::Result<()> {
        surf::post(format!("{}/send-tx", self.wallet_url))
            .body(serde_json::to_vec(&transaction).unwrap())
            .await?;
        Ok(())
    }
}
