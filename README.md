# ECSN: Embedded Compact State Node

A Utreexo compact state node for microcontrollers, in `no_std` Rust (with `alloc`), built on
[Floresta](https://github.com/getfloresta/Floresta)'s `floresta-chain`. The target is an STM32H7
with external SDRAM; this first step runs on a computer.

The node keeps the Utreexo accumulator (a few kilobytes) instead of the UTXO set. It downloads
headers, then each block with the Utreexo proof of the outputs it spends, from a peer serving
BIP 183 proofs (utreexod with a proof index, or Floresta), and validates it with
`floresta-chain`. Scripts are not checked yet (`floresta-chain` without `bitcoinkernel`).

## Crates

| Crate | |
|---|---|
| `ecsn` | `no_std`: the P2P messages (v1 transport, BIP 183's `getuproof`/`uproof`), a chain store in RAM, the node (handshake, header sync, block validation, following new blocks) over any `embedded-io-async` stream |
| `ecsn-host` | the node over TCP on a computer (tokio) |
| `ecsn-regtest` | fills a regtest node with blocks spending outputs of earlier blocks and of their own |

`floresta-chain` comes by path from `../bitcoin/Floresta`, branch `floresta-chain-no-std`, which builds it
without `std` (getfloresta/Floresta#1498).

## Environment

devenv (stable Rust, the `thumbv7em-none-eabihf` target, the ARM C compiler for
`secp256k1-sys`), loaded by direnv, or `devenv shell -- <command>`.

```
cargo test -p ecsn
cargo build -p ecsn --target thumbv7em-none-eabihf --release
cargo fmt -p ecsn -p ecsn-host -p ecsn-regtest   # not --all: it reformats ../bitcoin/Floresta
```

## Against a regtest bridge node

utreexod (`bitcoin/utreexod`, built with `CGO_ENABLED=0 go build`) with a proof index, unpruned
(a pruned node serves no proofs), mining to the test address of `ecsn-regtest address`:

```
utreexod --regtest --configfile=<dir>/utreexod.conf --datadir=<dir>/data --logdir=<dir>/logs \
  --utreexoproofindex --prune=0 --listen=127.0.0.1:18444 --rpclisten=127.0.0.1:18334 \
  --rpcuser=user --rpcpass=pass --notls --miningaddr=bcrt1ql3e9pgs3mmwuwrh95fecme0s0qtn2880hlwwpw
ecsn-regtest fill --blocks 10 --pairs 4
ecsn-host --network regtest            # or --follow to keep following new blocks
```

(`--configfile` naming an empty file keeps utreexod from creating `~/.utreexod`.) `ecsn-host`
prints the validated tip and its accumulator; utreexod's `getutreexoroots <hash>` gives its own
to compare.

`ecsn/tests/data/regtest-111.txt` is such a chain recorded from utreexod (blocks 1 to 111, block
111's Utreexo data and the accumulator after it), which the tests validate and tamper with.

## Status

- Checked on regtest against utreexod 0.6.0: 123 blocks, the accumulator identical to
  utreexod's; new blocks followed; changed proof hashes, amounts, heights, targets and missing
  leaves refused.
- One block at a time, one peer, the v1 transport (no BIP 324), no mempool, no script check,
  headers and accumulators in RAM.
