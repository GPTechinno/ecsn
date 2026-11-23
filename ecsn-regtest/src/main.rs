//! Fills a regtest node (utreexod with a proof index, or Bitcoin Core) with blocks whose
//! transactions spend outputs of earlier blocks and of the same block, so a Utreexo node has
//! proofs to check.
//!
//! The node mines to the address `address` prints (utreexod: `--miningaddr`), from a fixed
//! test key.

use std::process::ExitCode;

use base64::Engine;
use bitcoin::absolute::LockTime;
use bitcoin::consensus::encode::{deserialize_hex, serialize_hex};
use bitcoin::hashes::Hash;
use bitcoin::secp256k1::{All, Message, Secp256k1, SecretKey};
use bitcoin::sighash::{EcdsaSighashType, SighashCache};
use bitcoin::transaction::Version;
use bitcoin::{
    ecdsa, Address, Amount, Block, CompressedPublicKey, Network, OutPoint, PrivateKey, ScriptBuf,
    Sequence, Transaction, TxIn, TxOut, Witness,
};
use clap::{Parser, Subcommand};
use serde_json::{json, Value};

#[derive(Parser)]
struct Args {
    /// The node's JSON-RPC server, host:port, over plain HTTP
    #[arg(long, default_value = "127.0.0.1:18334")]
    rpc: String,
    #[arg(long, default_value = "user")]
    user: String,
    #[arg(long, default_value = "pass")]
    pass: String,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Prints the address the node mines to
    Address,
    /// Mines until coinbases are mature, then mines blocks of spending transactions
    Fill {
        /// Blocks of transactions to mine
        #[arg(long, default_value_t = 10)]
        blocks: u32,
        /// Transaction pairs per block (a parent of one or two inputs and a child spending it)
        #[arg(long, default_value_t = 4)]
        pairs: usize,
    },
}

const FEE: Amount = Amount::from_sat(2_000);

struct Wallet {
    secp: Secp256k1<All>,
    key: SecretKey,
    public: CompressedPublicKey,
    script: ScriptBuf,
}

impl Wallet {
    fn new() -> Self {
        let secp = Secp256k1::new();
        let key = SecretKey::from_slice(&[0x11; 32]).expect("a valid key");
        let public =
            CompressedPublicKey::from_private_key(&secp, &PrivateKey::new(key, Network::Regtest))
                .expect("a compressed key");
        let script = ScriptBuf::new_p2wpkh(&public.wpubkey_hash());
        Self {
            secp,
            key,
            public,
            script,
        }
    }

    fn address(&self) -> Address {
        Address::p2wpkh(&self.public, Network::Regtest)
    }

    /// A transaction spending `inputs` into `outputs` equal outputs to us, plus `extra`.
    fn spend(
        &self,
        inputs: &[(OutPoint, Amount)],
        outputs: u64,
        extra: Option<TxOut>,
    ) -> Transaction {
        let total: Amount = inputs.iter().map(|(_, value)| *value).sum();
        let each = (total - FEE) / outputs;
        let mut output: Vec<TxOut> = (0..outputs)
            .map(|_| TxOut {
                value: each,
                script_pubkey: self.script.clone(),
            })
            .collect();
        output.extend(extra);
        let mut tx = Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: inputs
                .iter()
                .map(|(previous_output, _)| TxIn {
                    previous_output: *previous_output,
                    script_sig: ScriptBuf::new(),
                    sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
                    witness: Witness::new(),
                })
                .collect(),
            output,
        };
        let mut witnesses = Vec::new();
        let mut cache = SighashCache::new(&tx);
        for (index, (_, value)) in inputs.iter().enumerate() {
            let sighash = cache
                .p2wpkh_signature_hash(index, &self.script, *value, EcdsaSighashType::All)
                .expect("a sighash");
            let message = Message::from_digest(sighash.to_byte_array());
            let signature =
                ecdsa::Signature::sighash_all(self.secp.sign_ecdsa(&message, &self.key));
            witnesses.push(Witness::p2wpkh(&signature, &self.public.0));
        }
        for (input, witness) in tx.input.iter_mut().zip(witnesses) {
            input.witness = witness;
        }
        tx
    }
}

struct Rpc {
    url: String,
    auth: String,
}

impl Rpc {
    fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        let body = json!({"jsonrpc": "1.0", "id": 1, "method": method, "params": params});
        let reply: Value = match ureq::post(&self.url)
            .set("Authorization", &self.auth)
            .send_json(body)
        {
            Ok(response) => response.into_json().map_err(|e| e.to_string())?,
            Err(ureq::Error::Status(_, response)) => {
                response.into_json().map_err(|e| e.to_string())?
            }
            Err(e) => return Err(format!("{method}: {e}")),
        };
        if !reply["error"].is_null() {
            return Err(format!("{method}: {}", reply["error"]));
        }
        Ok(reply["result"].clone())
    }

    fn height(&self) -> Result<u32, String> {
        self.call("getblockcount", json!([]))?
            .as_u64()
            .map(|h| h as u32)
            .ok_or_else(|| "getblockcount: not a number".into())
    }

    fn block(&self, height: u32) -> Result<Block, String> {
        let hash = self.call("getblockhash", json!([height]))?;
        let hex = self.call("getblock", json!([hash, 0]))?;
        let hex = hex.as_str().ok_or("getblock: not a string")?;
        deserialize_hex(hex).map_err(|e| format!("block {height}: {e}"))
    }

    fn mine(&self, blocks: u32) -> Result<(), String> {
        self.call("generate", json!([blocks])).map(|_| ())
    }

    fn send(&self, tx: &Transaction) -> Result<(), String> {
        self.call("sendrawtransaction", json!([serialize_hex(tx)]))
            .map(|_| ())
    }
}

/// Our unspent outputs, coinbases only once mature.
fn unspent(rpc: &Rpc, wallet: &Wallet) -> Result<Vec<(OutPoint, Amount)>, String> {
    let tip = rpc.height()?;
    let mut outputs = Vec::new();
    let mut spent = std::collections::HashSet::new();
    for height in 1..=tip {
        let block = rpc.block(height)?;
        for (i, tx) in block.txdata.iter().enumerate() {
            let coinbase = i == 0;
            if !coinbase {
                spent.extend(tx.input.iter().map(|input| input.previous_output));
            }
            // spendable in the next block
            if coinbase && height + 100 > tip + 1 {
                continue;
            }
            let txid = tx.compute_txid();
            for (vout, output) in tx.output.iter().enumerate() {
                if output.script_pubkey == wallet.script {
                    outputs.push((OutPoint::new(txid, vout as u32), output.value));
                }
            }
        }
    }
    outputs.retain(|(outpoint, _)| !spent.contains(outpoint));
    Ok(outputs)
}

fn fill(rpc: &Rpc, wallet: &Wallet, blocks: u32, pairs: usize) -> Result<(), String> {
    let height = rpc.height()?;
    if height < 110 {
        rpc.mine(110 - height)?;
    }
    for _ in 0..blocks {
        let mut utxos = unspent(rpc, wallet)?;
        let mut sent = 0;
        for pair in 0..pairs {
            // one input, then two, in turn
            let take = if pair % 2 == 1 && utxos.len() >= 2 {
                2
            } else {
                1
            };
            if utxos.len() < take {
                break;
            }
            let inputs: Vec<_> = utxos.drain(..take).collect();
            let parent = wallet.spend(&inputs, 3, None);
            // spends the parent's first output in the same block, with an unspendable output
            let child_input = (
                OutPoint::new(parent.compute_txid(), 0),
                parent.output[0].value,
            );
            let child = wallet.spend(
                &[child_input],
                2,
                Some(TxOut {
                    value: Amount::ZERO,
                    script_pubkey: ScriptBuf::new_op_return(b"ecsn"),
                }),
            );
            rpc.send(&parent)?;
            rpc.send(&child)?;
            sent += 2;
        }
        rpc.mine(1)?;
        println!("block {}: {sent} transactions", rpc.height()?);
    }
    Ok(())
}

fn main() -> ExitCode {
    let args = Args::parse();
    let wallet = Wallet::new();
    let rpc = Rpc {
        url: format!("http://{}/", args.rpc),
        auth: format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD
                .encode(format!("{}:{}", args.user, args.pass))
        ),
    };
    let result = match args.command {
        Command::Address => {
            println!("{}", wallet.address());
            Ok(())
        }
        Command::Fill { blocks, pairs } => fill(&rpc, &wallet, blocks, pairs),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}
