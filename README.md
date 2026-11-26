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
| `ecsn-emu-disco` | the STM32H747I-DISCO emulated (`emulator/stm32h7-rax` with the board around it), to run and debug the firmware: a BLIT window or headless. Its own workspace, built from its directory with BLIT's nightly Rust |
| `ecsn-firmware` (`firmware/`) | the node on an STM32H747I-DISCO (Cortex-M7): embassy, Ethernet with DHCP, the 32 MB SDRAM as the heap, the chain on the 128 MB QSPI flash. Its own workspace, built from its directory |

`floresta-chain` comes by git from the `perf-chain-walks` branch of the fork
[GPTechinno/Floresta](https://github.com/GPTechinno/Floresta): upstream's `master`, which builds it
without `std` since getfloresta/Floresta#1501, and its walks back along the chain that no longer hash
headers, not upstream yet.

## Environment

devenv (stable Rust, the `thumbv7em-none-eabihf` target, the ARM C compiler for
`secp256k1-sys`, probe-rs), loaded by direnv, or `devenv shell -- <command>`.

```
cargo test -p ecsn
cargo build -p ecsn --target thumbv7em-none-eabihf --release
cargo fmt --all
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

- header records (92 bytes: the header without the previous block's hash, which is its
  parent's, the header's hash, its parent's record, its height and a commit mark), appended:
  lookups compare stored hashes, and a header is hashed only when saved;
- a hash table from block hashes to records and an index from heights to records, both made of
  slots written once;
- a journal of the best chain, the index entries changed by reorgs and the headers marked
  invalid, in two banks used in turn;
- the accumulators of the latest blocks, in a ring of erase blocks.

The state of each header (valid, header only, in a fork) is kept in RAM and, after a reset,
follows from the index and the best chain. 128 MB hold 1,296,810 headers. A power cut at any
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
- `ECSN_NTP`: the SNTP server, a name or an IPv4 address, `pool.ntp.org` by default. The node asks
  it the time at the start and every 5 minutes (every 30 s until it answers) for its clock and its
  `version` messages; until then the display shows the peer's time and `version` carries the
  build's.

The display is drawn with BLIT, taken by git from upstream,
[nicoburniske/blit](https://github.com/nicoburniske/blit), pinned at a revision, built without
`std`. Its glyphs are rendered at build time from the DejaVu fonts in the directory `ECSN_FONTS`,
which the devenv sets.

The peer must accept the board's connection: run utreexod with `--listen` on a LAN address, not
`127.0.0.1`. LD1 (green) blinks while the firmware runs. LD2 (orange) is on while it is
connected.

The firmware logs at its start what reset the chip (power-on, the reset pin, the watchdog,
software). Its watchdog (IWDG1, 32 s) is armed once the node reaches the block sync, past the
one-off steps that hold the core longer; from then on a core held 32 s resets the board, which
resumes from the flash. The executor's stalls and the heap's peak are logged as they grow.

The 4" display (800 × 480, MIPI DSI) shows, drawn with BLIT, the step the firmware is at while it
starts (DHCP, the chain store), then the node, redrawn every second: the validated height out of the best header with
its progress, the validated tip (hash, time, age, transactions, inputs proven, size), the node's
phase, the peer (address, user agent, height), the board's address and MAC, the flash and heap in
use, the accumulator. The clock is the peer's, taken from its `version` message: the board has no
clock set. The firmware tells the panel's controller (OTM8009A on the first boards, NT35510 on later
ones) from its ID, and runs without the display if neither answers.

The board's Ethernet needs a hardware change. With the default solder bridges, PC1 is both
ETH_MDC and SAI4_D1 of the digital microphones (UM2411). Zephyr's documentation of the board
gives the change: open SB21, SB45 and R87, and close SB22, SB44, SB17 and SB8.

The chain is kept on the QSPI flash: two MT25QL512 in dual-flash mode, one 128 MB flash read
through the memory map. The first start formats it, which erases what it holds (ST's demonstration
data among it) and takes up to a few minutes.

## The emulated board

[`ecsn-emu-disco/README.md`](ecsn-emu-disco/README.md) has a screenshot and every option.

`ecsn-emu-disco` runs the firmware's ELF on the STM32H7 of [stm32h7-rax](https://github.com/GPTechinno/stm32h7-rax) with the
DISCO's 32 MB SDRAM, its two MT25QL512 (128 MB in dual-flash mode, kept in a file across runs, one
per network: `<network>.bin` in `$XDG_DATA_HOME/ecsn-emu-disco` or `~/.local/share/ecsn-emu-disco`,
another file with `--qspi`, none with `--volatile`), its LAN8742A and a network behind it: esp32s3-rax's router, with DHCP, DNS and NAT, where
the gateway 10.0.2.2 is the host's 127.0.0.1; and its display: the panel on the DSI link (an
OTM8009A, `--panel nt35510` for the later controller) showing what the LTDC sends while the panel is
awake and on and its backlight lit. The firmware is built with that peer:

```
cd firmware && ECSN_PEER=10.0.2.2:18444 cargo build --release    # utreexod on the host, port 18444
cd ../ecsn-emu-disco && cargo run --release                          # the window
cargo run --release -- --headless --seconds 30 --realtime --volatile  # logs on stdout, an erased flash
```

The flash image is read at the start (an erased flash if the file is missing) and written at the
end, and at each RESET from the window, so a run resumes the chain store the last one left, as the
board does after a reboot. The store records its network, and a firmware for another network
formats the flash for its own, so the emulator keeps a file per network, picked from the
firmware's ELF (its `ECSN_NETWORK_MAGIC` symbol); `--qspi` forces another. The window shows the board, from the photograph of its
front on ST's product page (`ecsn-emu-disco/assets`), with what the display shows in the display's
place and its four user LEDs (the column left of the RESET button, LD1 at the top) shining where
they are lit (DISPLAY 1:1 shows the display alone, at its size); under it what the LEDs mean (LD1
blinks while the firmware runs, LD2 is on while connected to the peer, LD3 and LD4 are not used),
the address, the validated tip, the pace and a RESET button; beside them the decoded defmt logs,
scrolled sideways by the bar under them, a sideways scroll or Shift and the wheel. Headless: `--screen-png FILE` saves what
the display shows at the end, `--realtime` keeps virtual time no faster than the wall clock (for
real peers; a board fallen behind, its core busy, does not catch up), `--profile SECONDS` samples
the PC every millisecond and prints the busiest functions with their callers for each interval of
that many seconds, and for the whole run at the end, `--trace` prints the PC and the core clock
every millisecond, `--mmio-trace start:end` every peripheral access in a range. Ctrl-C (or SIGTERM)
ends a headless run as `--seconds` does: the statistics, the profile, the QSPI image saved.
`ECSN_LOG=debug DEFMT_LOG=debug` at the firmware's build gives the node's debug logs. Its devenv
(nightly Rust from `rust-toolchain.toml`, the fonts, the Wayland libraries) is its own.

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
- The firmware runs on the emulated board (not yet on a real one): SDRAM, Ethernet with DHCP, the
  chain store formatted and mounted on the QSPI flash, synced and following utreexod, restarted and
  reorged; and on signet with assume-utreexo, from an erased flash to the tip, through the public
  peer. The emulator found four bugs of the node, fixed with their tests: an early header
  announcement taken for the answer to `getheaders`, a block announced during a sync forgotten, a
  reset during the header sync forcing a reindex of the whole chain, and a header sync idling
  between batches (now pipelined, 5 times faster on the board). Its display works there too, with
  either controller.
