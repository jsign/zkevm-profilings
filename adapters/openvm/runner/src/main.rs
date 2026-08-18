use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::Instant,
};

use anyhow::{bail, Context, Result};
use clap::Parser;
use guest_workload::{Input, Output};
use openvm_sdk::{
    config::AggregationSystemParams, prover::verify_app_proof, DefaultStarkEngine, Sdk, StdIn,
};
use openvm_stark_sdk::{
    bench::run_with_metric_collection,
    config::{app_params_with_100_bits_security, MAX_APP_LOG_STACKED_HEIGHT},
};
use profile_schema::{
    parse_openvm_metrics_with_symbols, write_flamegraph, write_folded, write_json, AdapterStatus,
    AdapterSummary, Metric,
};
use serde_json::Value;
use sha2::{Digest, Sha256};

const OPENVM_PROVENANCE: &str = "v2.1.0-preview@538c548";
const GUEST_RUST_TARGET: &str = "riscv64im-unknown-openvm-elf";
const GUEST_RUST_TOOLCHAIN: &str = "openvm-1.94.1";
const ELF_CLASS_64: u8 = 2;
const ELF_DATA_LITTLE_ENDIAN: u8 = 1;
const ELF_MACHINE_RISCV: u16 = 243;

#[derive(Clone, Debug)]
struct FunctionBound {
    start: u32,
    end: u32,
    name: String,
}

#[derive(Debug, Parser)]
struct Args {
    #[arg(long)]
    input: PathBuf,
    #[arg(long)]
    out: PathBuf,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let timer = Instant::now();
    let input: Input = serde_json::from_slice(&fs::read(&args.input)?)?;
    let expected_output = guest_workload::run(input);
    let adapter_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("locate OpenVM adapter root")?;
    let guest_manifest = adapter_root.join("guest/Cargo.toml");
    let build_target = args.out.join("guest-target");
    let executable_directory = args.out.join("build");
    fs::create_dir_all(&executable_directory)?;
    let build_command = vec![
        "cargo".to_owned(),
        "openvm".to_owned(),
        "build".to_owned(),
        "--manifest-path".to_owned(),
        guest_manifest.display().to_string(),
        "--target-dir".to_owned(),
        build_target.display().to_string(),
        "--output-dir".to_owned(),
        executable_directory.display().to_string(),
        "--locked".to_owned(),
    ];
    let build = Command::new(&build_command[0])
        .args(&build_command[1..])
        .env("OPENVM_RUSTC_TARGET", GUEST_RUST_TARGET)
        .env("OPENVM_RUST_TOOLCHAIN", GUEST_RUST_TOOLCHAIN)
        .current_dir(adapter_root)
        .output()
        .context("build OpenVM guest with cargo-openvm")?;
    fs::write(args.out.join("openvm-build.stdout.log"), &build.stdout)?;
    fs::write(args.out.join("openvm-build.stderr.log"), &build.stderr)?;
    if !build.status.success() {
        bail!(
            "cargo openvm build failed with {}: {}",
            build.status,
            String::from_utf8_lossy(&build.stderr)
        );
    }
    let elf_path = build_target.join(format!("{GUEST_RUST_TARGET}/release/profile-openvm-guest"));
    let elf_bytes =
        fs::read(&elf_path).with_context(|| format!("read guest ELF {}", elf_path.display()))?;
    validate_riscv64_elf(&elf_bytes)?;

    let app_params = app_params_with_100_bits_security(MAX_APP_LOG_STACKED_HEIGHT);
    let sdk = Sdk::riscv64(app_params, AggregationSystemParams::default());
    let guest_symbols_path = args.out.join("guest-symbols.bin");
    std::env::set_var("GUEST_SYMBOLS_PATH", &guest_symbols_path);
    // cargo-openvm's default build does not enable function-span metadata in its .vmexe.
    // Transpile the ELF in this runner, whose openvm-sdk dependency enables perf-metrics.
    let exe = sdk
        .convert_to_exe(elf_bytes.clone())
        .context("transpile OpenVM guest ELF with function spans")?;
    let function_bounds = exe
        .fn_bounds
        .values()
        .map(|bound| FunctionBound {
            start: bound.start,
            end: bound.end,
            name: bound.name.clone(),
        })
        .collect::<Vec<_>>();
    let mut stdin = StdIn::default();
    stdin.write(&input);
    let (public_values, (trace_cells, instructions)) = {
        let compiled = sdk
            .compile_metered_cost(exe.clone())
            .context("compile OpenVM guest for metered execution")?;
        sdk.execute_metered_cost(&compiled, stdin.clone())
            .context("run OpenVM metered execution")?
    };
    let output = decode_public_output(&public_values)?;
    if output != expected_output {
        bail!("OpenVM metered execution returned the wrong public output");
    }

    // OpenVM v2.1.0-preview supplies metered execution.
    // Its public AppProver API does not supply a mutable preflight-only operation.
    // The preview no longer emits its function-level cells_used counters during proof preflight.
    // Collect the dynamic function spans through the public interpreter preflight, then apportion
    // the exact metered trace-cell total by retired instructions. The stable fallback still
    // completes an app proof and makes sure that the proof is correct.
    let metrics_path = args.out.join("metrics.json");
    let mode_path = args.out.join("profile-mode.json");
    write_json(
        &serde_json::json!({
            "requested": "preflight-only",
            "available": false,
            "selected": "full-proof-fallback",
            "reason": "OpenVM v2.1.0-preview has no public API that stops AppProver after preflight",
            "function_cost_attribution": "instruction-weighted-estimate",
            "function_metric_compatibility": "cells_used records synthesized because preview commit 538c548 does not emit function-level cells_used counters"
        }),
        &mode_path,
    )?;
    println!("OpenVM profile mode: full-proof-fallback (v2.1.0-preview has no public preflight-only API)");
    std::env::set_var("OUTPUT_PATH", &metrics_path);
    let mut function_counts = None;
    run_with_metric_collection("OUTPUT_PATH", || -> Result<()> {
        let (_, app_vk) = sdk.app_keygen();
        let mut prover = sdk
            .app_prover(exe)
            .context("create OpenVM app prover")?
            .with_program_name("profile-openvm-guest");
        let preflight_state = prover
            .vm()
            .create_initial_state(prover.exe().as_ref(), stdin.clone());
        let preflight_interpreter = prover
            .vm()
            .preflight_interpreter(prover.exe().as_ref())
            .context("create OpenVM profiling preflight interpreter")?;
        let preflight = prover
            .vm()
            .execute_preflight(&preflight_interpreter, preflight_state)
            .context("run OpenVM profiling preflight")?;
        function_counts = Some(count_function_spans(
            preflight.history.program.iter().map(|event| event.pc),
            &function_bounds,
        ));
        let proof = prover.prove(stdin).context("generate OpenVM app proof")?;
        let _ = verify_app_proof::<DefaultStarkEngine>(&app_vk, &proof)
            .context("verify OpenVM app proof")?;
        Ok(())
    })?;
    let mut metrics: Value = serde_json::from_slice(&fs::read(&metrics_path)?)?;
    append_compatibility_cells_used(
        &mut metrics,
        function_counts.context("OpenVM profiling preflight did not return function counts")?,
        trace_cells,
    )?;
    write_json(&metrics, &metrics_path)?;
    let guest_symbols = fs::read(&guest_symbols_path)
        .with_context(|| format!("read OpenVM guest symbols {}", guest_symbols_path.display()))?;
    let profile = parse_openvm_metrics_with_symbols(&metrics, "cells_used", &guest_symbols)?;
    write_folded(&profile, &args.out.join("stacks.folded"))?;
    write_flamegraph(
        &profile,
        &args.out.join("flamegraph.svg"),
        "OpenVM trace-cell profile",
        "trace cells",
    )?;

    let artifacts = BTreeMap::from([
        ("metrics".to_owned(), "openvm/metrics.json".to_owned()),
        (
            "profile-mode".to_owned(),
            "openvm/profile-mode.json".to_owned(),
        ),
        (
            "guest-symbols".to_owned(),
            "openvm/guest-symbols.bin".to_owned(),
        ),
        (
            "folded-stacks".to_owned(),
            "openvm/stacks.folded".to_owned(),
        ),
        ("flamegraph".to_owned(), "openvm/flamegraph.svg".to_owned()),
        (
            "build-stdout".to_owned(),
            "openvm/openvm-build.stdout.log".to_owned(),
        ),
        (
            "build-stderr".to_owned(),
            "openvm/openvm-build.stderr.log".to_owned(),
        ),
    ]);
    let proof_command = vec![
        "openvm-sdk".to_owned(),
        "AppProver::prove".to_owned(),
        "--verify".to_owned(),
    ];
    let summary = AdapterSummary {
        vm: "openvm".to_owned(),
        status: AdapterStatus::Success,
        sdk_version: OPENVM_PROVENANCE.to_owned(),
        tool_version: OPENVM_PROVENANCE.to_owned(),
        profile_mode: "full-proof-fallback".to_owned(),
        duration_ms: timer.elapsed().as_millis() as u64,
        commands: vec![build_command, proof_command],
        primary_metric: Some(Metric::new(
            "estimated-trace-cells",
            "trace cells",
            trace_cells,
        )),
        secondary_metrics: vec![Metric::new("instructions", "instructions", instructions)],
        output: Some(output),
        output_digest: Some(digest_output(&output)?),
        elf_sha256: Some(hex::encode(Sha256::digest(&elf_bytes))),
        top_self: profile.top_self(20),
        top_inclusive: profile.top_inclusive(20),
        artifacts,
        error: None,
    };
    write_json(&summary, &args.out.join("summary.json"))?;
    Ok(())
}

fn count_function_spans(
    pcs: impl IntoIterator<Item = u32>,
    bounds: &[FunctionBound],
) -> BTreeMap<String, u64> {
    let mut counts = BTreeMap::new();
    let mut current: Option<usize> = None;
    let mut stack = Vec::<String>::new();

    for pc in pcs {
        let current_contains_pc = current
            .and_then(|index| bounds.get(index))
            .is_some_and(|bound| (bound.start..=bound.end).contains(&pc));
        if !current_contains_pc {
            current = bounds.iter().rposition(|bound| bound.start <= pc);
            if let Some(bound) = current.and_then(|index| bounds.get(index)) {
                if pc == bound.start {
                    stack.push(bound.name.clone());
                } else {
                    while stack.last() != Some(&bound.name) {
                        if stack.pop().is_none() {
                            break;
                        }
                    }
                }
            } else {
                stack.clear();
            }
        }

        let span = if stack.is_empty() {
            "[unattributed]".to_owned()
        } else {
            stack.join(";")
        };
        *counts.entry(span).or_default() += 1;
    }
    counts
}

fn append_compatibility_cells_used(
    metrics: &mut Value,
    function_counts: BTreeMap<String, u64>,
    trace_cells: u64,
) -> Result<()> {
    let counters = metrics
        .get_mut("counter")
        .and_then(Value::as_array_mut)
        .context("OpenVM metrics JSON has no counter array")?;
    if counters
        .iter()
        .any(|entry| entry.get("metric").and_then(Value::as_str) == Some("cells_used"))
    {
        return Ok(());
    }
    if trace_cells == 0 {
        bail!("OpenVM metered execution returned zero trace cells");
    }
    let total_instructions = function_counts.values().try_fold(0_u64, |total, count| {
        total
            .checked_add(*count)
            .context("OpenVM function instruction count overflow")
    })?;
    if total_instructions == 0 {
        bail!("OpenVM profiling preflight returned no instructions");
    }

    let count_len = function_counts.len();
    let mut allocated = 0_u64;
    for (index, (span, instruction_count)) in function_counts.into_iter().enumerate() {
        let cells = if index + 1 == count_len {
            trace_cells - allocated
        } else {
            let cells = ((trace_cells as u128) * (instruction_count as u128)
                / (total_instructions as u128)) as u64;
            allocated += cells;
            cells
        };
        if cells == 0 {
            continue;
        }
        counters.push(serde_json::json!({
            "labels": [
                ["cycle_tracker_span", span],
                ["source", "preview-instruction-weighted-estimate"]
            ],
            "metric": "cells_used",
            "value": cells.to_string()
        }));
    }
    Ok(())
}

fn validate_riscv64_elf(bytes: &[u8]) -> Result<()> {
    if bytes.len() < 20 {
        bail!(
            "OpenVM guest ELF is truncated: expected at least 20 bytes, found {}",
            bytes.len()
        );
    }
    if &bytes[..4] != b"\x7fELF" {
        bail!("OpenVM guest output is not an ELF file");
    }
    if bytes[4] != ELF_CLASS_64 {
        bail!(
            "OpenVM guest ELF is not 64-bit: ELF class byte is {}",
            bytes[4]
        );
    }
    if bytes[5] != ELF_DATA_LITTLE_ENDIAN {
        bail!(
            "OpenVM guest ELF is not little-endian: ELF data byte is {}",
            bytes[5]
        );
    }
    let machine = u16::from_le_bytes([bytes[18], bytes[19]]);
    if machine != ELF_MACHINE_RISCV {
        bail!("OpenVM guest ELF does not target RISC-V: ELF machine is {machine}");
    }
    Ok(())
}

fn decode_public_output(bytes: &[u8]) -> Result<Output> {
    if bytes.len() < 32 {
        bail!(
            "OpenVM returned {} public bytes; expected at least 32",
            bytes.len()
        );
    }
    let mut values = [0_u64; 4];
    for (index, value) in values.iter_mut().enumerate() {
        let start = index * 8;
        *value = u64::from_le_bytes(bytes[start..start + 8].try_into().unwrap());
    }
    Ok(Output {
        state: [values[0], values[1], values[2]],
        checksum: values[3],
    })
}

fn digest_output(output: &Output) -> Result<String> {
    Ok(hex::encode(Sha256::digest(serde_json::to_vec(output)?)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn riscv64_elf_header() -> Vec<u8> {
        let mut bytes = vec![0_u8; 20];
        bytes[..4].copy_from_slice(b"\x7fELF");
        bytes[4] = ELF_CLASS_64;
        bytes[5] = ELF_DATA_LITTLE_ENDIAN;
        bytes[18..20].copy_from_slice(&ELF_MACHINE_RISCV.to_le_bytes());
        bytes
    }

    #[test]
    fn accepts_riscv64_elf() {
        validate_riscv64_elf(&riscv64_elf_header()).unwrap();
    }

    #[test]
    fn rejects_elf32() {
        let mut bytes = riscv64_elf_header();
        bytes[4] = 1;
        assert!(validate_riscv64_elf(&bytes)
            .unwrap_err()
            .to_string()
            .contains("not 64-bit"));
    }

    #[test]
    fn rejects_big_endian_elf() {
        let mut bytes = riscv64_elf_header();
        bytes[5] = 2;
        assert!(validate_riscv64_elf(&bytes)
            .unwrap_err()
            .to_string()
            .contains("not little-endian"));
    }

    #[test]
    fn rejects_truncated_elf() {
        assert!(validate_riscv64_elf(b"\x7fELF")
            .unwrap_err()
            .to_string()
            .contains("truncated"));
    }

    #[test]
    fn rejects_non_riscv_elf() {
        let mut bytes = riscv64_elf_header();
        bytes[18..20].copy_from_slice(&62_u16.to_le_bytes());
        assert!(validate_riscv64_elf(&bytes)
            .unwrap_err()
            .to_string()
            .contains("does not target RISC-V"));
    }

    #[test]
    fn reconstructs_dynamic_function_spans() {
        let bounds = vec![
            FunctionBound {
                start: 0,
                end: 16,
                name: "root".to_owned(),
            },
            FunctionBound {
                start: 20,
                end: 40,
                name: "child".to_owned(),
            },
        ];
        let counts = count_function_spans([0, 4, 20, 24, 28, 8], &bounds);
        assert_eq!(counts["root"], 3);
        assert_eq!(counts["root;child"], 3);
    }

    #[test]
    fn compatibility_cells_preserve_metered_total() {
        let mut metrics = serde_json::json!({"counter": []});
        append_compatibility_cells_used(
            &mut metrics,
            BTreeMap::from([("root".to_owned(), 2), ("root;child".to_owned(), 1)]),
            10,
        )
        .unwrap();
        let total = metrics["counter"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["value"].as_str().unwrap().parse::<u64>().unwrap())
            .sum::<u64>();
        assert_eq!(total, 10);
    }
}
