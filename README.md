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
| `ecsn` | `no_std`: the P2P messages (v1 transport, BIP 183's `getuproof`/`uproof`), chain stores in RAM and on NOR flash, the node (handshake, header sync, block validation, following new blocks) over any `embedded-io-async` stream |
| `ecsn-host` | the node over TCP on a computer (tokio), its chain in RAM or in a flash image file |
| `ecsn-regtest` | fills a regtest node with blocks spending outputs of earlier blocks and of their own |
| `ecsn-firmware` (`firmware/`) | the node on an STM32H747I-DISCO (Cortex-M7): embassy, Ethernet with DHCP, the 32 MB SDRAM as the heap, the chain on the 128 MB QSPI flash. Its own workspace, built from its directory |

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

While syncing, the node asks for `--blocks-in-flight` blocks at once (16 by default, 4 on the
board), the proof of each as it comes, and validates them in order; it flushes the chain every
100 blocks.

(`--configfile` naming an empty file keeps utreexod from creating `~/.utreexod`.) `ecsn-host`
prints the validated tip and its accumulator; utreexod's `getutreexoroots <hash>` gives its own
to compare.

With `--store <file>`, `ecsn-host` keeps the chain in the flash chain store, in a file holding the
image of a NOR flash (created erased, 128 MiB by default, `--store-size`). A later run on the same
file starts from what it holds.

`ecsn/tests/data/regtest-111.txt` is such a chain recorded from utreexod (blocks 1 to 111, block
111's Utreexo data and the accumulator after it), which the tests validate and tamper with.

## The flash chain store

`ecsn::store::FlashChainStore` keeps the chain on NOR flash (`ecsn::flash::Flash`: reads, programs
that only clear bits, erases of whole blocks):

- header records (88 bytes: the header, its height and a commit mark), appended;
- a hash table from block hashes to records and an index from heights to records, both made of
  slots written once;
- a journal of the best chain, the index entries changed by reorgs and the headers marked
  invalid, in two banks used in turn;
- the accumulators of the latest blocks, in a ring of erase blocks.

The state of each header (valid, header only, in a fork) is kept in RAM and, after a reset,
follows from the index and the best chain. 128 MB hold 1,353,193 headers. A power cut at any
point leaves a store that mounts and resumes (`ecsn/tests/flash_store.rs` cuts the power at
every 17th programmed byte of a run, in `ecsn::flash::RamFlash`).

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
- `ECSN_RAM_STORE`: set it to keep the chain in RAM instead of on the QSPI flash.
- `ECSN_LOG`: the log level, `info` by default.

The peer must accept the board's connection: run utreexod with `--listen` on a LAN address, not
`127.0.0.1`. LD1 (green) blinks while the firmware runs. LD2 (orange) is on while it is
connected.

The board's Ethernet needs a hardware change. With the default solder bridges, PC1 is both
ETH_MDC and SAI4_D1 of the digital microphones (UM2411). Zephyr's documentation of the board
gives the change: open SB21, SB45 and R87, and close SB22, SB44, SB17 and SB8.

The chain is kept on the QSPI flash: two MT25QL512 in dual-flash mode, one 128 MB flash read
through the memory map. The first start formats it, which erases what it holds (ST's demonstration
data among it) and takes up to a few minutes.

## Status

- Checked on regtest against utreexod 0.6.0: 123 blocks, the accumulator identical to
  utreexod's; new blocks followed; changed proof hashes, amounts, heights, targets and missing
  leaves refused.
- The flash chain store, in a flash image: synced, restarted (only the new blocks validated),
  reorged by utreexod (`invalidateblock`) and restarted again, the accumulator identical to
  utreexod's at each step.
- Blocks pipelined: windows of 1 and 16 give utreexod's accumulator on 150 regtest blocks; the
  tests also check blocks sent in reverse (`ecsn/tests/sync.rs`, a peer answering from the
  fixture).
- One peer, the v1 transport (no BIP 324), no mempool, no script check.
- The firmware builds; it has not run on a board yet.
