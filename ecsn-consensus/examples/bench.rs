//! Times the script checks of a block's transactions: `bench <file>`, each line a transaction in
//! hex, a space, then the outputs it spends as `script_hex:satoshis`, comma-separated (from
//! Bitcoin Core's `getblock <hash> 3`).

use std::time::Instant;

use bitcoin::consensus::encode::{deserialize_hex, serialize};
use bitcoin::{Amount, ScriptBuf, Transaction, TxOut};

fn main() {
    let path = std::env::args().nth(1).expect("a file");
    let text = std::fs::read_to_string(path).expect("readable");
    let mut rows = Vec::new();
    let start = Instant::now();
    for line in text.lines() {
        let (hex, spent) = line.split_once(' ').expect("two fields");
        let tx: Transaction = deserialize_hex(hex).expect("a transaction");
        let spent: Vec<TxOut> = spent
            .split(',')
            .map(|s| {
                let (script, sats) = s.split_once(':').expect("script:sats");
                TxOut {
                    script_pubkey: ScriptBuf::from_hex(script).expect("hex"),
                    value: Amount::from_sat(sats.parse().expect("sats")),
                }
            })
            .collect();
        let bytes = serialize(&tx);
        let t = Instant::now();
        let result = ecsn_consensus::verify_scripts(&bytes, &spent, 0x20e15);
        let us = t.elapsed().as_micros();
        assert!(result.is_ok(), "{}: {result:?}", tx.compute_txid());
        rows.push((us, tx.input.len(), bytes.len()));
    }
    let total: u128 = rows.iter().map(|r| r.0).sum();
    let inputs: usize = rows.iter().map(|r| r.1).sum();
    println!(
        "{} transactions, {inputs} inputs: {} ms checking ({} ms in all)",
        rows.len(),
        total / 1000,
        start.elapsed().as_millis()
    );
    let single: Vec<_> = rows.iter().filter(|r| r.1 == 1).collect();
    let per_single = single.iter().map(|r| r.0).sum::<u128>() / single.len().max(1) as u128;
    println!(
        "{} transactions of one input: {per_single} us each",
        single.len()
    );
    rows.sort_by_key(|r| std::cmp::Reverse(r.0));
    for (us, n, size) in rows.iter().take(8) {
        println!("  {us} us: {n} inputs, {size} bytes");
    }
}
