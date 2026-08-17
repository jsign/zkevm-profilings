use std::{collections::BTreeMap, fs, path::PathBuf, time::Instant};

use anyhow::{Context, Result};
use clap::Parser;
use guest_workload::{Input, Output};
use profile_schema::{
    read_firefox_profile, write_flamegraph, write_folded, write_json, AdapterStatus,
    AdapterSummary, Metric,
};
use sha2::{Digest, Sha256};
use sp1_sdk::{include_elf, Elf, Prover, ProverClient, SP1Stdin};

const ELF: Elf = include_elf!("profile-sp1-guest");
const SDK_VERSION: &str = "6.4.0";

#[derive(Debug, Parser)]
struct Args {
    #[arg(long)]
    input: PathBuf,
    #[arg(long)]
    out: PathBuf,
    #[arg(long, default_value_t = 1)]
    sample_rate: u32,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let timer = Instant::now();
    let input: Input = serde_json::from_slice(&fs::read(&args.input)?)?;
    let trace_path = args.out.join("profile.json");

    // The SP1 executor reads these variables when it makes the profiler.
    std::env::set_var("TRACE_FILE", &trace_path);
    std::env::set_var("TRACE_SAMPLE_RATE", args.sample_rate.to_string());

    let mut stdin = SP1Stdin::new();
    stdin.write(&input);
    let client = ProverClient::from_env().await;
    let (mut public_values, report) = client
        .execute(ELF.clone(), stdin)
        .calculate_gas(true)
        .await
        .context("execute SP1 guest")?;
    let output = public_values.read::<Output>();

    write_json(&report, &args.out.join("execution-report.json"))?;
    let profile = read_firefox_profile(&trace_path, u64::from(args.sample_rate))?;
    if profile.folded.is_empty() {
        anyhow::bail!("SP1 produced an empty profile");
    }
    write_folded(&profile, &args.out.join("stacks.folded"))?;
    write_flamegraph(
        &profile,
        &args.out.join("flamegraph.svg"),
        "SP1 execution profile",
        "cycles",
    )?;

    let cycles = report.total_instruction_count() + report.total_syscall_count();
    let mut secondary_metrics = vec![
        Metric::new("prover-gas", "gas", report.gas().unwrap_or(0)),
        Metric::new(
            "instructions",
            "instructions",
            report.total_instruction_count(),
        ),
        Metric::new("syscalls", "calls", report.total_syscall_count()),
    ];
    secondary_metrics.retain(|metric| metric.value > 0);
    let artifacts = BTreeMap::from([
        ("profile".to_owned(), "sp1/profile.json".to_owned()),
        (
            "execution-report".to_owned(),
            "sp1/execution-report.json".to_owned(),
        ),
        ("folded-stacks".to_owned(), "sp1/stacks.folded".to_owned()),
        ("flamegraph".to_owned(), "sp1/flamegraph.svg".to_owned()),
    ]);
    let summary = AdapterSummary {
        vm: "sp1".to_owned(),
        status: AdapterStatus::Success,
        sdk_version: SDK_VERSION.to_owned(),
        tool_version: SDK_VERSION.to_owned(),
        profile_mode: "execute".to_owned(),
        duration_ms: timer.elapsed().as_millis() as u64,
        commands: vec![vec![
            "sp1-sdk".to_owned(),
            "ProverClient::execute".to_owned(),
            "--calculate-gas".to_owned(),
        ]],
        primary_metric: Some(Metric::new("cycles", "cycles", cycles)),
        secondary_metrics,
        output: Some(output),
        output_digest: Some(digest_output(&output)?),
        elf_sha256: Some(hex::encode(Sha256::digest(&*ELF))),
        top_self: profile.top_self(20),
        top_inclusive: profile.top_inclusive(20),
        artifacts,
        error: None,
    };
    write_json(&summary, &args.out.join("summary.json"))?;
    Ok(())
}

fn digest_output(output: &Output) -> Result<String> {
    Ok(hex::encode(Sha256::digest(serde_json::to_vec(output)?)))
}
