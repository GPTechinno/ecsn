# ECSN: Embedded Compact State Node

A Utreexo compact state node for microcontrollers, in `no_std` Rust (with `alloc`), built on
[Floresta](https://github.com/getfloresta/Floresta)'s `floresta-chain`. The target is an STM32H7
with external SDRAM. The node runs on a computer, and is being brought up on an STM32H747I-DISCO
(`firmware/`).

The node keeps the Utreexo accumulator (a few kilobytes) instead of the UTXO set. It downloads
headers, then each block with the Utreexo proof of the outputs it spends, from a peer serving
BIP 183 proofs (utreexod with a proof index, or Floresta), and validates it with
`floresta-chain`, and its scripts with Bitcoin Core's script interpreter (`ecsn-consensus`) when
the node is given it: on a computer by default, on the board with the firmware's `scripts`
feature.

## Crates

| Crate | |
|---|---|
| `ecsn` | `no_std`: the P2P messages (the v1 transport and BIP 324's v2, BIP 183's `getuproof`/`uproof`/`utreexotx`, BIP 155's `addrv2`), chain stores in RAM and on NOR flash, the address book and the DNS seeds, a mempool of transactions checked with their Utreexo proofs, the node over any `embedded-io-async` stream: a bridge's connection (handshake, header sync, block validation, following new blocks, the transactions it relays) and ordinary peers' (headers, block announcements, addresses), each in a slot that reconnects to another address when it ends |
| `ecsn-consensus` | the scripts of a transaction checked by Bitcoin Core 26.0's script interpreter (libbitcoinconsensus, C++, every input in one call), the function the node takes; `no_std`, for the host and the firmware |
| `ecsn-host` | the node over TCP on a computer (tokio), its chain in RAM or in a flash image file, scripts checked (`--no-scripts`), its address book in a file (`--book`) |
| `ecsn-regtest` | fills a regtest node with blocks spending outputs of earlier blocks and of their own |
| `ecsn-mempool` | records the transactions and blocks the network relays, raw, with their time |
| `ecsn-replay` | replays such a recording as a mempool on the board's heap (its TLSF allocator, 32-bit, 32 MiB), built for `wasm32-wasip1`, to measure how the heap gets cut up |
| `ecsn-emu-disco` | the STM32H747I-DISCO emulated (`emulator/stm32h7-rax` with the board around it), to run and debug the firmware: a BLIT window or headless. Its own workspace, built from its directory with BLIT's nightly Rust |
| `ecsn-firmware` (`firmware/`) | the node on an STM32H747I-DISCO (Cortex-M7): embassy, Ethernet with DHCP, the 32 MB SDRAM as the heap, the chain on the 128 MB QSPI flash. Its own workspace, built from its directory |

`floresta-chain` comes by git from the `perf-header-sync` branch of the fork
[GPTechinno/Floresta](https://github.com/GPTechinno/Floresta): upstream's `master`, which builds it
without `std` since getfloresta/Floresta#1501, its walks back along the chain that no longer hash
headers (branch `perf-chain-walks`), and its header acceptance hashing each header once, not
upstream yet. The firmware also takes `bitcoin_hashes` 0.14.101 from the `hashes-sha256-extern`
branch of [GPTechinno/rust-bitcoin](https://github.com/GPTechinno/rust-bitcoin) (a `[patch]`): its
SHA-256 compression is then the firmware's `bitcoin_hashes_sha256_compress`, which the emulator
runs on the host. Likewise `bitcoin_hashes` 1.1.0, whose SHA-512 makes the accumulator's hashes
(through rustreexo), from the fork's `hashes-1.1-sha512-extern` branch with its `sha512-extern`
feature: `bitcoin_hashes_sha512_compress`.

The workspace and the firmware take `rustreexo` 0.7.0 from the `proof-memory` branch of
[GPTechinno/rustreexo](https://github.com/GPTechinno/rustreexo) (a `[patch]`): it checks a proof
without copying its hashes into a sorted list nor reserving room for twice as many computed nodes,
which took about 21 MB for some mainnet blocks and does not fit in the board's heap.

`ecsn-consensus` takes `bitcoinconsensus` (rust-bitcoinconsensus, Bitcoin Core 26.0's
libbitcoinconsensus) from the `verify-transaction` branch of the fork
[GPTechinno/rust-bitcoinconsensus](https://github.com/GPTechinno/rust-bitcoinconsensus) (a
`[patch]`): `verify_transaction` checks every input of a transaction in one call
(the transaction deserialized and its signature hashes' shared data computed once, where the
library's per-input calls redo both for each input: a transaction of 740 inputs in 28 ms instead of
605 ms on a computer), the crate builds without `std`, for `arm-none-eabi` (newlib's `int32_t`), and
with a smaller table of signature verification (`BITCOINCONSENSUS_ECMULT_WINDOW_SIZE`, 8 for the
firmware: 8 KiB instead of 1 MiB). Building it needs a C++ compiler: the host's, and
`arm-none-eabi-g++` for the firmware (the devenv's `CXX_thumbv7em_none_eabihf`).

## Scripts

A block's scripts are checked after its Utreexo proof (which gives the outputs it spends, those
of its own transactions taken from them) and before it is connected; a mempool transaction's after
its amounts. The flags follow the chain's soft forks as Bitcoin Core's: P2SH, segwit and Taproot
from the genesis block but in the two mainnet blocks that break them, DERSIG, CHECKLOCKTIMEVERIFY,
CHECKSEQUENCEVERIFY and NULLDUMMY from their heights (`ecsn::scripts::ScriptRules`).

The firmware checks them with its `scripts` feature (`cargo build --release --features scripts`
from `firmware/`). It links Arm's C++ runtime (libstdc++ and libsupc++ nano, newlib nano, libgcc:
`arm-none-eabi-g++`'s multilib for the Cortex-M7 with its FPU), serves `malloc` from the heap, runs
the C++ static constructors at the start (the Taproot hashers among them; cortex-m-rt runs none),
and keeps the exception tables the interpreter unwinds with: a copy of cortex-m-rt's `link.x`
without its discard of them (`link-cxx.x`), `cxx.x` placing them, and PC-relative type references
in them (`--target2=rel`). The firmware then takes 1.1 MB of flash: both banks (`memory.x.in`, 2 MB
with the feature), the Cortex-M4's bank 2 included, which on a real DISCO means keeping the M4 from
booting (its option bytes).

## Environment

devenv (stable Rust, the `thumbv7em-none-eabihf` and `wasm32-wasip1` targets, the ARM C compiler
and an unwrapped clang for `secp256k1-sys` on them, probe-rs, wasmtime), loaded by direnv, or
`devenv shell -- <command>`.

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

While syncing, the node asks for `--blocks-in-flight` blocks at once (16 by default, 2 on the
board, where one validated while the next comes keeps it busy), the proof of each as it comes, and
validates them in order; it saves the chain after each block validated. When the heap left is
known (the board's, or under `--heap-limit`), it asks for fewer: beyond the first block in flight,
one more only if the heap left keeps 8 MiB free for a validation and the decoding of a block, plus
4 MiB for each block asked and not here yet, and a free block of 2 MiB (a heap in small pieces
fails the allocations of a validation). Each connection receives into one buffer, kept from one
message to the next and across the slot's connections, grown to the largest message: a large
buffer freed after each block would cut the heap up.

With `--follow` the node keeps a connection to a bridge (the peer given first, then any address
whose services include `NODE_UTREEXO_ARCHIVE`) and `--peers` (2 by default) to ordinary peers, each
in a slot that reconnects to another address when it ends. The addresses come from the network's
DNS seeds and from the peers (`getaddr`, `addr`, `addrv2`); a peer that sends invalid data waits a
day, one never reached is forgotten after three tries. The ordinary peers announce new blocks:
their headers are checked and added to the header chain, and the bridge's connection fetches the
blocks once the bridge has them (a `notfound` from it waits for its announcement). `ecsn-host`
logs the connections every minute. IPv4 only unless `--ipv6`.

The v2 transport (BIP 324: a key exchange, then encrypted packets) is tried with the peers whose
services include `NODE_P2P_V2` and with the configured bridge; when its handshake fails the slot
connects again at once in v1, and keeps to v1 with that address. utreexod speaks it, with the
one-byte IDs 29 and 30 for `uproof` and `getuproof`. `--v1` never tries it; without `--follow`,
`--v2` speaks it with the peer.

(`--configfile` naming an empty file keeps utreexod from creating `~/.utreexod`.) `ecsn-host`
prints the validated tip and its accumulator; utreexod's `getutreexoroots <hash>` gives its own
to compare.

With `--store <file>`, `ecsn-host` keeps the chain in the flash chain store, in a file holding the
image of a NOR flash (created erased, 128 MiB by default, `--store-size`). A later run on the same
file starts from what it holds. The emulator's flash images are such files.

`ecsn-host` logs the heap's peak as it grows by a MiB, the bytes allocated at once at most, and
`--heap-limit <MiB>` fails the allocations beyond that many MiB, as the board's 32 MB of SDRAM
does (its structures take more there, with 64-bit pointers), and fits the blocks in flight to it.

`ecsn/tests/data/regtest-111.txt` is such a chain recorded from utreexod (blocks 1 to 111, block
111's Utreexo data and the accumulator after it), which the tests validate and tamper with.

## The mempool

Once synced, the node asks the bridge for its transactions (`relay` and `NODE_UTREEXO` in its
`version`, toward the bridge only). As utreexod does for a Utreexo peer, the bridge announces each
transaction with the positions in the accumulator of the leaves it spends; the node asks for it
as a `utreexotx` with the positions of the proof hashes it needs (it caches none), once, and
checks what comes against the accumulator of its
validated tip (`ecsn::mempool`): the proof, the outputs spent (in the accumulator, or of
transactions it holds), the amounts, the maturity of the coinbase outputs spent, the lock times;
not the scripts, as for blocks. The proof is then dropped: a transaction stays valid until a block
confirms it or spends what it spends, which the block's inputs tell, and a block off the tip it
was checked on (a reorganisation) empties the mempool. A transaction spending what another one
held spends replaces it, and what spends from it, only paying more in all and per vbyte; one
spending outputs of a transaction not held waits aside for it until the next block. Beyond the cap
(`--mempool`, MB of serialized transactions; `ECSN_MEMPOOL` on the board) the lowest fee rates
leave first, with what spends from them; on the board no transaction is asked for while the heap
is short. The mempool is off by default (below).

utreexod 0.5.1 changes its own copy of a transaction each time it sends it as a `utreexotx` (the
output indexes of its inputs shifted): the node asks for each transaction once, and takes only
those whose txid it asked for. The bridge's later sends of that transaction, to anyone, carry the
changed copy, which ordinary peers asking for it as a `tx` take for an invalid transaction and may
punish the bridge for. So the mempool is off unless asked for (`--mempool 4` with `--follow`,
`ECSN_MEMPOOL=4` for the firmware) until bridges carry a fix.

## A mempool on the board's heap

Before the mempool, a measurement: how the board's heap gets cut up by thousands of transactions
coming and going beside the blocks. `ecsn-mempool` records what a node would keep, from a few
peers of the DNS seeds asked to relay transactions (no validation): each transaction and block,
raw, with its time. `ecsn-replay` replays a recording with every allocation in a TLSF allocator
(rlsf, with the firmware's parameters) over a 32 MiB arena, 32-bit as the board (`wasm32-wasip1`
under wasmtime): each transaction kept until a block confirms it or conflicts with it (or another
spends the same output), the oldest leaving beyond the cap; each block's payload, decoded, held with
a proof's room, then dropped. It prints the heap, its free bytes and its largest free block at each
block and every 2,000 transactions, then a summary.

```
ecsn-mempool --network bitcoin --peers 3 --minutes 120 --out mainnet.rec
cargo build -p ecsn-replay --release --target wasm32-wasip1
wasmtime run --dir . target/wasm32-wasip1/release/ecsn-replay.wasm mainnet.rec 8   # cap, MiB
wasmtime run --dir . target/wasm32-wasip1/release/ecsn-replay.wasm mainnet.rec 8 raw
```

`raw` keeps each transaction as its bytes with the outputs it spends instead of decoded. On two
hours of mainnet (2026-11-27: 29,333 transactions, 16 MB, 7 blocks), from an empty mempool: no
block's payload lacked a free block (the largest at least 11 MB at a 4 MiB cap, 8 to 12 % of the
free bytes in smaller ones at each block), and the mempool took 2.2 to 2.9 bytes of heap a byte
whether decoded or raw (raw saves 8 to 10 %): its indexes (by txid, by output spent, the arrival
order) cost as much as its transactions.

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
ST-LINK with probe-rs, with its logs over defmt RTT. The bridge tried first is set at build time:

```
cd firmware
ECSN_PEER=192.168.1.10:18444 cargo run --release
```

Optional build-time settings:

- `ECSN_NETWORK`: `regtest` by default.
- `ECSN_PEERS`: the ordinary peers kept connected, 2 by default (with the bridge, as `ecsn-host
  --follow` does; the DNS seeds through the network's DNS).
- `ECSN_V1`: set it to keep to the v1 transport; by default the v2 (BIP 324) is tried as
  `ecsn-host --follow` does, its keys from the chip's RNG.
- `ECSN_V1_BRIDGE`: set it to keep the bridge alone in the v1 transport. In v2 its blocks cost their
  decryption, about 6 % of the core while syncing (the `chacha20-poly1305` crate built at
  `opt-level = 3`; a quarter at the profile's `"s"`).
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
`127.0.0.1`. LD1 (green) blinks while the firmware runs. LD2 (orange) is on while a bridge is
connected.

The firmware logs at its start what reset the chip (power-on, the reset pin, the watchdog,
software). Its watchdog (IWDG1, 32 s) is armed once the node reaches the block sync, past the
one-off steps that hold the core longer; from then on a core held 32 s resets the board, which
resumes from the flash. The executor's stalls and the heap's peak are logged as they grow, the heap's largest free block as it shrinks.

The 4" display (800 × 480, MIPI DSI) shows, drawn with BLIT, the step the firmware is at while it
starts (DHCP, the chain store), then the node, redrawn every second: the validated height out of the best header with
its progress, the validated tip (hash, time, age, transactions, inputs proven, size), the node's
phase, the peers (the bridge and the ordinary peers: address, client, `v2` when encrypted, the
addresses known),
the board's address and MAC, the flash and the RAM in use (the SDRAM, the heap), the accumulator. Until SNTP answers, the
clock is the bridge's, taken from its `version` message. The firmware tells the panel's controller (OTM8009A on the first boards, NT35510 on later
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
blinks while the firmware runs, LD2 is on while connected to a bridge, LD3 and LD4 are not used),
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
- A bridge without the blocks of the best chain for 20 minutes is left for another
  (`ecsn/tests/peers.rs`, tokio's paused time); the address book is saved and loaded across
  restarts (the board: the erase block the chain store leaves at the end of the flash, loaded on the
  emulated board; the host: `--book`).
- One bridge at a time and a few ordinary peers (`ecsn/tests/peers.rs`; checked on mainnet from
  the host and the emulated board), the v1 and v2 transports (`ecsn/src/p2p.rs`'s tests: messages
  both ways, a 162 kB message received with its receive dropped every millisecond, a v1 peer; the
  bridge and Bitcoin Core 32.1 peers in v2 on mainnet).
- Scripts (`ecsn/tests/regtest.rs` and `mempool.rs`: block 111's and its transactions' pass, a
  changed signature with the same outputs spent and the same proof is refused; mainnet blocks from
  the host, about 0.4 s of a computer's core for 8,000 inputs).
- The mempool (`ecsn/tests/mempool.rs`: block 111's transactions with the parts of the block's
  proof that prove what each spends, tampered with, with their conflicts, orphans, the cap and the
  block; a bridge in the same process relaying them as utreexod does); on mainnet from the host
  against the bridge (utreexod 0.5.1: 1,417 transactions taken and 1 refused in 5 minutes),
  and on the emulated board.
- The firmware runs on the emulated board (not yet on a real one): SDRAM, Ethernet with DHCP, the
  chain store formatted and mounted on the QSPI flash, synced and following utreexod, restarted and
  reorged; and on signet with assume-utreexo, from an erased flash to the tip, through the public
  peer. The emulator found four bugs of the node, fixed with their tests: an early header
  announcement taken for the answer to `getheaders`, a block announced during a sync forgotten, a
  reset during the header sync forcing a reindex of the whole chain, and a header sync idling
  between batches (now pipelined, 5 times faster on the board). Its display works there too, with
  either controller.
