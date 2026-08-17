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

const SDK_VERSION: &str = "2.0.2";

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
    ];
    let build = Command::new(&build_command[0])
        .args(&build_command[1..])
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
    let elf_path = build_target.join("riscv32im-risc0-zkvm-elf/release/profile-openvm-guest");
    let elf_bytes =
        fs::read(&elf_path).with_context(|| format!("read guest ELF {}", elf_path.display()))?;

    let app_params = app_params_with_100_bits_security(MAX_APP_LOG_STACKED_HEIGHT);
    let sdk = Sdk::riscv32(app_params, AggregationSystemParams::default());
    let guest_symbols_path = args.out.join("guest-symbols.bin");
    std::env::set_var("GUEST_SYMBOLS_PATH", &guest_symbols_path);
    // cargo-openvm's default build does not enable function-span metadata in its .vmexe.
    // Transpile the ELF in this runner, whose openvm-sdk dependency enables perf-metrics.
    let exe = sdk
        .convert_to_exe(elf_bytes.clone())
        .context("transpile OpenVM guest ELF with function spans")?;
    let mut stdin = StdIn::default();
    stdin.write(&input);
    let (public_values, (trace_cells, instructions)) = sdk
        .execute_metered_cost(exe.clone(), stdin.clone())
        .context("run OpenVM metered execution")?;
    let output = decode_public_output(&public_values)?;
    if output != expected_output {
        bail!("OpenVM metered execution returned the wrong public output");
    }

    // OpenVM 2.0.2 supplies metered execution.
    // Its public AppProver API does not supply a mutable preflight-only operation.
    // Preflight supplies the cells_used metric for each function.
    // Thus, the stable fallback completes an app proof and makes sure that the proof is correct.
    let metrics_path = args.out.join("metrics.json");
    let mode_path = args.out.join("profile-mode.json");
    write_json(
        &serde_json::json!({
            "requested": "preflight-only",
            "available": false,
            "selected": "full-proof-fallback",
            "reason": "OpenVM 2.0.2 has no released public API that stops AppProver after preflight"
        }),
        &mode_path,
    )?;
    println!("OpenVM profile mode: full-proof-fallback (v2.0.2 has no public preflight-only API)");
    std::env::set_var("OUTPUT_PATH", &metrics_path);
    run_with_metric_collection("OUTPUT_PATH", || -> Result<()> {
        let (_, app_vk) = sdk.app_keygen();
        let mut prover = sdk
            .app_prover(exe)
            .context("create OpenVM app prover")?
            .with_program_name("profile-openvm-guest");
        let proof = prover.prove(stdin).context("generate OpenVM app proof")?;
        let _ = verify_app_proof::<DefaultStarkEngine>(&app_vk, &proof)
            .context("verify OpenVM app proof")?;
        Ok(())
    })?;
    let metrics: Value = serde_json::from_slice(&fs::read(&metrics_path)?)?;
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
        sdk_version: SDK_VERSION.to_owned(),
        tool_version: SDK_VERSION.to_owned(),
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
