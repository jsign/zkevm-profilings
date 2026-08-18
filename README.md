# Native multi-zkVM profiling harness

This repository profiles one Rust workload on OpenVM, SP1, and ZisK. It does not use Docker.
Each adapter uses the applicable native Rust toolchain and SDK.

Default profile operations run all three adapters.

The harness pins these releases:

- [OpenVM v2.1.0-preview](https://github.com/openvm-org/openvm/tree/v2.1.0-preview) at commit `538c548`
- [SP1 v6.4.0](https://github.com/succinctlabs/sp1/releases/tag/v6.4.0)
- [ZisK v1.1.0-alpha](https://github.com/0xPolygonHermez/zisk/releases/tag/v1.1.0-alpha)

Each adapter has a separate `Cargo.lock` file and target directory. The root workspace resolves its
dependencies separately from the vendor dependencies.

OpenVM v2.1.0-preview is an unreleased RV64 snapshot. Its crates and CLI still report package
version `2.0.0`. This harness identifies the snapshot as `v2.1.0-preview@538c548` and checks the
CLI commit instead of its package version.

## Sample flamegraphs

These flamegraphs come from profile run `20260818-114647Z` with the supplied default fixture. Each
image links to the full-size SVG.

### OpenVM

[![OpenVM trace-cell flamegraph][openvm-sample]][openvm-sample]

### SP1

[![SP1 execution flamegraph][sp1-sample]][sp1-sample]

### ZisK

[![ZisK proof-area flamegraph][zisk-sample]][zisk-sample]

[openvm-sample]: assets/20260818-114647Z/openvm-flamegraph.svg
[sp1-sample]: assets/20260818-114647Z/sp1-flamegraph.svg
[zisk-sample]: assets/20260818-114647Z/zisk-flamegraph.svg

## Quick start

Before you start a default profile operation, use this command:

```text
cargo xtask doctor
```

The `doctor` command does not install or change software. It examines the operating system, exact
CLI versions, Rust toolchains, lockfiles, and common native build tools.

If it finds a problem, it gives an official installation command. Before you use that command,
examine it.

OpenVM v2.1.0-preview requires Rust 1.91.1 for the host and the `openvm-1.94.1` guest toolchain.
Install the pinned CPU CLI and guest toolchain with these commands:

```text
rustup toolchain install 1.91.1 --profile minimal
cargo +1.91.1 install --locked --force --git https://github.com/openvm-org/openvm.git --rev 538c5488130da56c8442d33445efe3c1fe5ea8b8 cargo-openvm
cargo openvm toolchain install
```

The preview supplies guest toolchains for x86-64 Linux, AArch64 Linux, and AArch64 macOS.

Install the pinned ZisK CPU CLI and guest toolchain with this command:

```text
curl https://raw.githubusercontent.com/0xPolygonHermez/zisk/main/ziskup/install.sh | bash -s -- --version 1.1.0-alpha --cpu --nokey
```

To profile all VMs with the supplied fixture, use this command:

```text
./scripts/profile-all --input fixtures/default.json
```

To profile one adapter, use this command:

```text
cargo xtask profile --vm sp1 --input fixtures/default.json
```

The output directory must not exist. If you do not use `--out`, the harness writes to
`profiles/<UTC timestamp>/`.

## Edit the workload

Edit [`crates/guest-workload/src/lib.rs`](crates/guest-workload/src/lib.rs). Only this file contains
workload logic. The crate uses `no_std`. It gives deterministic results and has no VM dependencies.

The initialization, execution kernels, and finalization functions use `#[inline(never)]`. Thus,
profiles show useful symbol frames for arithmetic, memory, branch, and division work. Do not put
vendor I/O, guest profile markers, or VM feature flags in this crate.

The wrapper for each guest does only these operations:

1. Read `Input { seed, rounds }` with the vendor API.
2. Call `guest_workload::run`.
3. Commit the `Output { state, checksum }` value.

After a workload change, use the native tests:

```text
cargo test --workspace
```

## Adapter behavior

### OpenVM

The adapter uses `cargo openvm build --locked` and `openvm.toml` to build an RV64IM ELF with target
`riscv64im-unknown-openvm-elf`. It rejects ELF32, big-endian, and non-RISC-V build output. The guest
publishes its four output words with `reveal_u64`.

The runner transpiles the ELF with the `perf-metrics` feature of the SDK. This step emits
`guest-symbols.bin`, which the profile parser uses to resolve numeric function-span offsets. It
uses `Sdk::riscv64`, compiles the executable with `compile_metered_cost`, and starts metered
execution. This operation supplies total instructions, estimated trace cells, and public output.

At preview commit `538c548`, proof preflight does not emit the function-level `cells_used`
counters that its profiling parser expects. The runner uses interpreter preflight to collect the
dynamic function-span instruction counts. It apportions the exact metered trace-cell total across
these spans by instruction count and writes compatibility `cells_used` records. Thus, the total is
exact, but per-function trace-cell costs are instruction-weighted estimates. The runner records
this detail in `profile-mode.json`.

The preview SDK does not supply a stable public operation that stops `AppProver` after preflight.
After preflight, its public `AppProver` starts proof generation.

Thus, the adapter records `full-proof-fallback`. It makes an app proof and makes sure that the proof
is correct. It keeps the profile and discards the proof. It writes the reason to
`profile-mode.json`.

Before it starts the fallback, the adapter makes sure that metered execution gives the correct
output. If the guest fails or the output differs, the adapter stops. These failures do not start a
proof.

### SP1

The adapter uses `sp1-build` to build the guest. It enables the `profiling` feature in `sp1-sdk`.
Then, it uses `ProverClient::execute` with gas calculation. `TRACE_FILE` contains the path to the
profile-operation directory.

The default `TRACE_SAMPLE_RATE` is `1`. Thus, the profile includes each sample. The adapter keeps
the Firefox profile and the complete SP1 execution report. The summary contains cycles, normalized
prover gas, instruction count, syscall count, and public output.

### ZisK

To check or profile only ZisK, select the adapter explicitly:

```text
cargo xtask doctor --vm zisk
cargo xtask profile --vm zisk --input fixtures/default.json --out /tmp/zisk-profile
```

The adapter uses `cargo-zisk` to build the guest. It uses `ZiskStdin` to make the native input file.
Then, it starts `ziskemu` with symbols, full statistics, top functions, a Firefox profile, and
ungrouped numbers. ZisK writes the statistics snapshot and HTML report with its native
`--save-stats` and `--html-report` options. The harness also keeps the complete emulator output.
The summary gets its totals from `stats.csv`. These totals include the initialization costs for ROM
and RAM.

## Output

A profile operation for all adapters has this structure:

```text
profiles/<run>/
├── input.json
├── run.json
├── summary.md
├── openvm/
│   ├── summary.json
│   ├── metrics.json
│   ├── stacks.folded
│   ├── flamegraph.svg
│   ├── stdout.log
│   └── stderr.log
├── sp1/
│   ├── summary.json
│   ├── profile.json
│   ├── stacks.folded
│   ├── flamegraph.svg
│   ├── stdout.log
│   └── stderr.log
└── zisk/
    ├── summary.json
    ├── profile.json.gz
    ├── stats.csv
    ├── report.html
    ├── stacks.folded
    ├── flamegraph.svg
    ├── stdout.log
    └── stderr.log
```

An adapter directory can contain more raw build and execution logs. `run.json` contains each
declared artifact, command, version, duration, hash, status, output digest, and profiling mode.

The default `--vm all` selection profiles OpenVM, SP1, and ZisK in that sequence. If one adapter
fails, the harness continues with the remaining selected adapters. It always writes `run.json` and
`summary.md`. If any adapter fails, the command exits with a nonzero status.

## Read the metrics correctly

Do not compare metrics between different VMs:

- OpenVM reports estimated trace cells and instruction counts.
- SP1 reports cycles and prover gas.
- ZisK reports steps and proof-area costs.

Do not make one score from these values. Compare the same workload and VM with an earlier version
that uses the same VM release.

Rust code uses `inferno` to generate SVG files. The harness does not need Python, Samply, or an
external flamegraph program. The SP1 and ZisK Firefox profiles remain available for use with
Firefox Profiler or Samply.

## Repository layout

```text
crates/guest-workload  VM-independent no_std logic
crates/profile-schema Profile parsers, summaries, folded stacks, and SVG output
crates/xtask          Doctor inspections and profile orchestration
adapters/openvm       Isolated OpenVM guest and runner
adapters/sp1          Isolated SP1 guest and runner
adapters/zisk         Isolated ZisK guest and runner
fixtures              Small committed inputs and parser fixtures
scripts/profile-all   Entry point for profiling all adapters
```

Git ignores generated profiles and all build directories. The repository contains fixtures and all
four lockfiles.
