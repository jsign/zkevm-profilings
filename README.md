# Native multi-zkVM profiling harness

This repository profiles one Rust workload on OpenVM, SP1, and ZisK. It does not use Docker.
Each adapter uses the applicable native Rust toolchain and SDK.

The harness pins these releases:

- [OpenVM v2.0.2](https://github.com/openvm-org/openvm/releases/tag/v2.0.2)
- [SP1 v6.4.0](https://github.com/succinctlabs/sp1/releases/tag/v6.4.0)
- [ZisK v1.0.0-alpha](https://github.com/0xPolygonHermez/zisk/releases/tag/v1.0.0-alpha)

Each adapter has a separate `Cargo.lock` file and target directory. The root workspace resolves its
dependencies separately from the vendor dependencies.

## Quick start

Before you start a profile operation, use this command:

```text
cargo xtask doctor --vm all
```

The `doctor` command does not install or change software. It examines the operating system, exact
CLI versions, Rust toolchains, lockfiles, and common native build tools.

If it finds a problem, it gives an official installation command. Before you use that command,
examine it.

To profile all VMs with the supplied fixture, use this command:

```text
./scripts/profile-all --input fixtures/default.json
```

To profile one adapter or select an output directory, use one of these commands:

```text
cargo xtask profile --vm sp1 --input fixtures/default.json
cargo xtask profile --vm zisk --input fixtures/default.json --out /tmp/zisk-profile
```

The output directory must not exist. If you do not use `--out`, the harness writes to
`profiles/<UTC timestamp>/`.

## Edit the workload

Edit [`crates/guest-workload/src/lib.rs`](crates/guest-workload/src/lib.rs). Only this file contains
workload logic. The crate uses `no_std`. It gives deterministic results and has no VM dependencies.

The `initialize`, `mix`, and `finalize` functions use `#[inline(never)]`. Thus, profiles show useful
symbol frames. Do not put vendor I/O, guest profile markers, or VM feature flags in this crate.

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

The adapter uses `cargo openvm build` and `openvm.toml` to build the guest. Then, it starts metered
execution. This operation supplies total instructions, estimated trace cells, and public output.
The runner transpiles the built ELF with the SDK's `perf-metrics` feature. This step emits
`guest-symbols.bin`, which the profile parser uses to resolve numeric function-span offsets.

During preflight, OpenVM v2.0.2 supplies `frequency` and `cells_used` metrics for each function.
The released SDK does not supply a public operation that only does preflight. After preflight, its
public `AppProver` starts proof generation.

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

The adapter uses `cargo-zisk` to build the guest. It uses `ZiskStdin` to make the native input file.
Then, it starts `ziskemu` with symbols, full statistics, top functions, a Firefox profile, and
ungrouped numbers.

ZisK v1.0.0-alpha does not have the later `--save-stats` or `--html-report` CLI options. The harness
keeps the complete emulator output. It uses this output to make `stats.csv` and `report.html`.
It does not replace or discard vendor data.

## Output

A complete profile operation has this structure:

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

The harness profiles adapters in this sequence: OpenVM, SP1, and ZisK. If one adapter fails, the
harness continues with the remaining selected adapters. It always writes `run.json` and
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
scripts/profile-all   Short entry point for cargo xtask profile
```

Git ignores generated profiles and all build directories. The repository contains fixtures and all
four lockfiles.
