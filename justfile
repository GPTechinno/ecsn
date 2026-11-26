# ECSN: the firmware for a network, on the emulated STM32H747I-DISCO or on a board.
#
# Networks: signet and mainnet (from floresta-chain's assumed accumulator, against the public
# Utreexo bridge 1.231.31.3), regtest (from genesis, against utreexod on the host: 10.0.2.2 from
# the emulated board); mainnet by default. `ECSN_PEER` in the environment replaces the peer. Each
# network has its own firmware build (`firmware/target/<network>`), and the emulator its own flash
# image (`<network>.bin` in `$XDG_DATA_HOME/ecsn-emu-disco`, picked from the firmware), since a
# chain store holds one network: `--qspi FILE` forces another file.
#
#   just emu                             mainnet, the window
#   just emu-headless signet 120         120 s of virtual time, logs on stdout
#   just emu signet --volatile           extra options go to the emulator
#   just build-fw                        the mainnet firmware alone
#   just flash signet                    on a board, through its ST-LINK (probe-rs)

set shell := ["bash", "-euo", "pipefail", "-c"]

elf := "thumbv7em-none-eabihf/release/ecsn-firmware"

# Lists the recipes.
default:
    @{{ just_executable() }} --list

# The firmware's build settings for `network`, as shell exports.
[private]
env network:
    #!/usr/bin/env bash
    set -euo pipefail
    case "{{ network }}" in
        signet) echo "export ECSN_NETWORK=signet ECSN_ASSUME_UTREEXO=1 ECSN_PEER=${ECSN_PEER:-1.231.31.3:38333}" ;;
        mainnet) echo "export ECSN_NETWORK=bitcoin ECSN_ASSUME_UTREEXO=1 ECSN_PEER=${ECSN_PEER:-1.231.31.3:8333}" ;;
        regtest) echo "export ECSN_NETWORK=regtest ECSN_PEER=${ECSN_PEER:-10.0.2.2:18444}" ;;
        *) echo "unknown network {{ network }}: signet, mainnet or regtest" >&2; exit 1 ;;
    esac

# Builds the firmware for `network` into firmware/target/<network>.
build-fw network="mainnet":
    #!/usr/bin/env bash
    set -euo pipefail
    eval "$({{ just_executable() }} env {{ network }})"
    devenv shell -- bash -c 'cd firmware && CARGO_TARGET_DIR=target/{{ network }} cargo build --release'

# Runs the firmware for `network` on the emulated board, in its window.
emu network="mainnet" *args: (build-fw network)
    cd ecsn-emu-disco && devenv shell -- cargo run --release -- --elf ../firmware/target/{{ network }}/{{ elf }} {{ args }}

# The same without a window, for `seconds` of virtual time kept to the wall clock.
emu-headless network="mainnet" seconds="60" *args: (build-fw network)
    cd ecsn-emu-disco && devenv shell -- cargo run --release -- --headless --realtime --seconds {{ seconds }} --elf ../firmware/target/{{ network }}/{{ elf }} {{ args }}

# Builds the firmware for `network` and runs it on a board through its ST-LINK (probe-rs).
flash network="mainnet":
    #!/usr/bin/env bash
    set -euo pipefail
    eval "$({{ just_executable() }} env {{ network }})"
    devenv shell -- bash -c 'cd firmware && CARGO_TARGET_DIR=target/{{ network }} cargo run --release'
