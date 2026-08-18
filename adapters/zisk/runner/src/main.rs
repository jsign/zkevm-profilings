use std::{collections::BTreeMap, fs, path::PathBuf, process::Command, time::Instant};

use anyhow::{bail, Context, Result};
use clap::Parser;
use guest_workload::{Input, Output};
use profile_schema::{
    parse_zisk_function_costs, parse_zisk_stats_csv, read_firefox_profile, write_flamegraph,
    write_folded, write_json, AdapterStatus, AdapterSummary, Metric,
};
use sha2::{Digest, Sha256};
use zisk_sdk::{load_program, GuestProgram, ZiskStdin};

static PROGRAM: GuestProgram = load_program!("profile-zisk-guest");
const SDK_VERSION: &str = "1.1.0-alpha";

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
    let elf_path = args.out.join("guest.elf");
    let input_path = args.out.join("input.bin");
    let output_path = args.out.join("output.bin");
    let profile_path = args.out.join("profile.json.gz");
    let stats_path = args.out.join("stats.csv");
    let report_path = args.out.join("report.html");
    fs::write(&elf_path, PROGRAM.elf())?;
    let stdin = ZiskStdin::new();
    stdin.write(&input);
    stdin.save(&input_path)?;

    let command = vec![
        "ziskemu".to_owned(),
        "-e".to_owned(),
        elf_path.display().to_string(),
        "-i".to_owned(),
        input_path.display().to_string(),
        "-o".to_owned(),
        output_path.display().to_string(),
        "-X".to_owned(),
        "-S".to_owned(),
        "--sdk".to_owned(),
        "--top-functions".to_owned(),
        "--no-thousands-sep".to_owned(),
        "--csv-separator=,".to_owned(),
        format!("--save-stats={}", stats_path.display()),
        format!("--html-report={}", report_path.display()),
        format!("--profiler-output={}", profile_path.display()),
    ];
    let process = Command::new(&command[0])
        .args(&command[1..])
        .output()
        .context("run ziskemu")?;
    fs::write(args.out.join("ziskemu.stdout.log"), &process.stdout)?;
    fs::write(args.out.join("ziskemu.stderr.log"), &process.stderr)?;
    if !process.status.success() {
        bail!(
            "ziskemu failed with {}: {}",
            process.status,
            String::from_utf8_lossy(&process.stderr)
        );
    }

    let output_bytes = fs::read(&output_path).context("read ZisK public output")?;
    let (output, _): (Output, usize) =
        bincode::serde::decode_from_slice(&output_bytes, bincode::config::standard())
            .context("decode ZisK public output")?;
    let stdout = String::from_utf8_lossy(&process.stdout);
    for path in [&stats_path, &report_path] {
        if !path.is_file() {
            bail!("ZisK did not produce {}", path.display());
        }
    }
    let statistics = parse_zisk_stats_csv(&fs::read_to_string(&stats_path)?);
    let total_cost = statistics.total_cost.context("ZisK snapshot has no TOTAL cost")?;
    let profile = read_firefox_profile(&profile_path, 1)?;
    if profile.folded.is_empty() {
        bail!("ZisK produced an empty profile");
    }
    write_folded(&profile, &args.out.join("stacks.folded"))?;
    write_flamegraph(
        &profile,
        &args.out.join("flamegraph.svg"),
        "ZisK proof-area profile",
        "cost",
    )?;
    let cumulative_functions = parse_zisk_function_costs(&stdout);

    let mut secondary_metrics = Vec::new();
    if let Some(value) = statistics.steps {
        secondary_metrics.push(Metric::new("steps", "steps", value));
    }
    if let Some(value) = statistics.base_cost {
        secondary_metrics.push(Metric::new("base-cost", "proof-area cost", value));
    }
    if let Some(value) = statistics.variable_cost {
        secondary_metrics.push(Metric::new("variable-cost", "proof-area cost", value));
    }
    let artifacts = BTreeMap::from([
        ("profile".to_owned(), "zisk/profile.json.gz".to_owned()),
        ("statistics".to_owned(), "zisk/stats.csv".to_owned()),
        ("html-report".to_owned(), "zisk/report.html".to_owned()),
        ("folded-stacks".to_owned(), "zisk/stacks.folded".to_owned()),
        ("flamegraph".to_owned(), "zisk/flamegraph.svg".to_owned()),
        (
            "emulator-stdout".to_owned(),
            "zisk/ziskemu.stdout.log".to_owned(),
        ),
        (
            "emulator-stderr".to_owned(),
            "zisk/ziskemu.stderr.log".to_owned(),
        ),
    ]);
    let summary = AdapterSummary {
        vm: "zisk".to_owned(),
        status: AdapterStatus::Success,
        sdk_version: SDK_VERSION.to_owned(),
        tool_version: SDK_VERSION.to_owned(),
        profile_mode: "emulator-full-statistics".to_owned(),
        duration_ms: timer.elapsed().as_millis() as u64,
        commands: vec![command],
        primary_metric: Some(Metric::new(
            "total-proof-area-cost",
            "proof-area cost",
            total_cost,
        )),
        secondary_metrics,
        output: Some(output),
        output_digest: Some(digest_output(&output)?),
        elf_sha256: Some(hex::encode(Sha256::digest(PROGRAM.elf()))),
        top_self: profile.top_self(20),
        top_inclusive: if cumulative_functions.is_empty() {
            profile.top_inclusive(20)
        } else {
            cumulative_functions.into_iter().take(20).collect()
        },
        artifacts,
        error: None,
    };
    write_json(&summary, &args.out.join("summary.json"))?;
    Ok(())
}

fn digest_output(output: &Output) -> Result<String> {
    Ok(hex::encode(Sha256::digest(serde_json::to_vec(output)?)))
}
