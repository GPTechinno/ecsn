# ECSN: Embedded Compact State Node

A Utreexo compact state node for microcontrollers, in `no_std` Rust (with `alloc`), built on
[Floresta](https://github.com/getfloresta/Floresta)'s `floresta-chain`. The target is an STM32H7
with external SDRAM. The node runs on a computer, and is being brought up on an STM32H747I-DISCO
(`firmware/`).

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
| `ecsn-firmware` (`firmware/`) | the node on an STM32H747I-DISCO (Cortex-M7): embassy, Ethernet with DHCP, the 32 MB SDRAM as the heap. Its own workspace, built from its directory |

`floresta-chain` comes by path from `../bitcoin/Floresta`, branch `floresta-chain-no-std`, which builds it
without `std` (getfloresta/Floresta#1498).

## Environment

devenv (stable Rust, the `thumbv7em-none-eabihf` target, the ARM C compiler for
`secp256k1-sys`, probe-rs), loaded by direnv, or `devenv shell -- <command>`.

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

## On the STM32H747I-DISCO

`firmware/` builds for `thumbv7em-none-eabihf`. It runs on the Cortex-M7 through the board's
ST-LINK with probe-rs, with its logs over defmt RTT. The peer is set at build time:

```
cd firmware
ECSN_PEER=192.168.1.10:18444 cargo run --release
```

Optional build-time settings:

- `ECSN_NETWORK`: `regtest` by default.
- `ECSN_ASSUME_UTREEXO`: set it to start from `floresta-chain`'s accumulator.
- `ECSN_LOG`: the log level, `info` by default.

The peer must accept the board's connection: run utreexod with `--listen` on a LAN address, not
`127.0.0.1`. LD1 (green) blinks while the firmware runs. LD2 (orange) is on while it is
connected.

The board's Ethernet needs a hardware change. With the default solder bridges, PC1 is both
ETH_MDC and SAI4_D1 of the digital microphones (UM2411). Zephyr's documentation of the board
gives the change: open SB21, SB45 and R87, and close SB22, SB44, SB17 and SB8.

The chain is kept in RAM: the headers, and an accumulator per block. That is enough for regtest.
Signet's and mainnet's headers alone outgrow the 32 MB SDRAM.

## Status

- Checked on regtest against utreexod 0.6.0: 123 blocks, the accumulator identical to
  utreexod's; new blocks followed; changed proof hashes, amounts, heights, targets and missing
  leaves refused.
- One block at a time, one peer, the v1 transport (no BIP 324), no mempool, no script check,
  headers and accumulators in RAM.
