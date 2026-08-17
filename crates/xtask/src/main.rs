use std::{
    env, fs,
    path::{Path, PathBuf},
    process::{Command, Output as ProcessOutput},
    time::Instant,
};

use anyhow::{bail, Context, Result};
use chrono::{SecondsFormat, Utc};
use clap::{Args, Parser, Subcommand, ValueEnum};
use guest_workload::{Input, Output};
use profile_schema::{write_json, AdapterStatus, AdapterSummary, RunManifest, SCHEMA_VERSION};
use sha2::{Digest, Sha256};

#[derive(Debug, Parser)]
#[command(about = "Build and profile the shared workload on supported zkVMs")]
struct Cli {
    #[command(subcommand)]
    command: Task,
}

#[derive(Debug, Subcommand)]
enum Task {
    /// Examine native tools without a system change.
    Doctor(DoctorArgs),
    /// Profile one or more VMs and collect their output files.
    Profile(ProfileArgs),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum VmSelection {
    All,
    Openvm,
    Sp1,
    Zisk,
}

impl VmSelection {
    fn members(self) -> Vec<Vm> {
        match self {
            Self::All => vec![Vm::Openvm, Vm::Sp1, Vm::Zisk],
            Self::Openvm => vec![Vm::Openvm],
            Self::Sp1 => vec![Vm::Sp1],
            Self::Zisk => vec![Vm::Zisk],
        }
    }
}

#[derive(Debug, Args)]
struct DoctorArgs {
    #[arg(long, value_enum, default_value_t = VmSelection::All)]
    vm: VmSelection,
}

#[derive(Debug, Args)]
struct ProfileArgs {
    #[arg(long, value_enum, default_value_t = VmSelection::All)]
    vm: VmSelection,
    /// This JSON file contains `seed` and `rounds`.
    #[arg(long)]
    input: PathBuf,
    /// This new output directory must not exist.
    #[arg(long)]
    out: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Vm {
    Openvm,
    Sp1,
    Zisk,
}

impl Vm {
    fn name(self) -> &'static str {
        match self {
            Self::Openvm => "openvm",
            Self::Sp1 => "sp1",
            Self::Zisk => "zisk",
        }
    }

    fn required_tools(self) -> &'static [ToolRequirement] {
        match self {
            Self::Openvm => &OPENVM_TOOLS,
            Self::Sp1 => &SP1_TOOLS,
            Self::Zisk => &ZISK_TOOLS,
        }
    }

    fn required_toolchain(self) -> &'static str {
        match self {
            Self::Openvm => "nightly-2026-01-18",
            Self::Sp1 => "succinct",
            Self::Zisk => "zisk",
        }
    }

    fn toolchain_install(self) -> &'static str {
        match self {
            Self::Openvm => "rustup toolchain install nightly-2026-01-18 --component rust-src",
            Self::Sp1 => "sp1up --version 6.4.0",
            Self::Zisk => "cargo-zisk toolchain install",
        }
    }

    fn native_dependencies(self) -> &'static [&'static str] {
        match self {
            Self::Openvm | Self::Sp1 => &["cc"],
            Self::Zisk => &["cc", "cmake", "clang", "pkg-config", "mpicc", "nasm"],
        }
    }
}

const VERSION_ARGUMENTS: &[&str] = &["--version"];
const OPENVM_TOOLS: [ToolRequirement; 1] = [ToolRequirement::new(
    "cargo-openvm",
    "cargo",
    &["openvm", "--version"],
    "2.0.2",
    "cargo install --git https://github.com/openvm-org/openvm.git --tag v2.0.2 cargo-openvm",
)];
const SP1_TOOLS: [ToolRequirement; 1] = [ToolRequirement::new(
    "cargo-prove",
    "cargo",
    &["prove", "--version"],
    "6.4.0",
    "curl -L https://sp1.succinct.xyz | bash && sp1up --version 6.4.0",
)];
const ZISK_TOOLS: [ToolRequirement; 2] = [
    ToolRequirement::new(
        "cargo-zisk",
        "cargo-zisk",
        VERSION_ARGUMENTS,
        "1.0.0-alpha",
        "curl https://raw.githubusercontent.com/0xPolygonHermez/zisk/main/ziskup/install.sh | bash -s -- --version 1.0.0-alpha --cpu --nokey",
    ),
    ToolRequirement::new(
        "ziskemu",
        "ziskemu",
        VERSION_ARGUMENTS,
        "1.0.0-alpha",
        "ziskup --version 1.0.0-alpha --cpu --nokey",
    ),
];

#[derive(Clone, Copy, Debug)]
struct ToolRequirement {
    label: &'static str,
    binary: &'static str,
    args: &'static [&'static str],
    version_fragment: &'static str,
    install: &'static str,
}

impl ToolRequirement {
    const fn new(
        label: &'static str,
        binary: &'static str,
        args: &'static [&'static str],
        version_fragment: &'static str,
        install: &'static str,
    ) -> Self {
        Self {
            label,
            binary,
            args,
            version_fragment,
            install,
        }
    }
}

#[derive(Debug)]
struct CheckResult {
    label: String,
    ok: bool,
    observed: String,
    remedy: Option<String>,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let repository = repository_root()?;
    match cli.command {
        Task::Doctor(args) => doctor(args.vm, &repository),
        Task::Profile(args) => profile(args, &repository),
    }
}

fn repository_root() -> Result<PathBuf> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .map(Path::to_path_buf)
        .context("locate repository root")
}

fn doctor(selection: VmSelection, repository: &Path) -> Result<()> {
    println!("Native multi-zkVM profiler doctor\n");
    let mut checks = vec![os_check(), binary_check("rustup", &["--version"], None)];
    println!("Host:");
    for check in &checks {
        print_check(check);
    }
    println!();
    for vm in selection.members() {
        println!("{}:", vm.name());
        for dependency in vm.native_dependencies() {
            let check = native_dependency_check(dependency);
            print_check(&check);
            checks.push(check);
        }
        for requirement in vm.required_tools() {
            let check = tool_check(*requirement);
            print_check(&check);
            checks.push(check);
        }
        let check = rust_toolchain_check(vm);
        print_check(&check);
        checks.push(check);
        let lockfile = repository
            .join("adapters")
            .join(vm.name())
            .join("Cargo.lock");
        let check = CheckResult {
            label: "adapter lockfile".to_owned(),
            ok: lockfile.is_file(),
            observed: lockfile.display().to_string(),
            remedy: Some(
                "Run `cargo generate-lockfile` in this adapter directory during development."
                    .to_owned(),
            ),
        };
        print_check(&check);
        checks.push(check);
        println!();
    }
    let failures = checks.iter().filter(|check| !check.ok).count();
    if failures > 0 {
        bail!("doctor found {failures} missing or incompatible prerequisites");
    }
    println!("All selected prerequisites are ready.");
    Ok(())
}

fn native_dependency_check(binary: &str) -> CheckResult {
    let mut check = binary_check(binary, &["--version"], None);
    let package = match binary {
        "cc" => "build-essential",
        "mpicc" => "libopenmpi-dev openmpi-bin",
        other => other,
    };
    check.remedy = Some(if env::consts::OS == "macos" {
        if binary == "cc" {
            "xcode-select --install".to_owned()
        } else {
            let brew_package = if binary == "mpicc" {
                "open-mpi"
            } else {
                binary
            };
            format!("brew install {brew_package}")
        }
    } else {
        format!("sudo apt-get install {package}")
    });
    check
}

fn print_check(check: &CheckResult) {
    let mark = if check.ok { "ok" } else { "missing" };
    println!("  [{mark}] {}: {}", check.label, check.observed);
    if !check.ok {
        if let Some(remedy) = &check.remedy {
            println!("          Install: {remedy}");
        }
    }
}

fn os_check() -> CheckResult {
    let os = env::consts::OS;
    CheckResult {
        label: "operating system".to_owned(),
        ok: matches!(os, "linux" | "macos"),
        observed: os.to_owned(),
        remedy: Some("Use CPU-native Linux or macOS.".to_owned()),
    }
}

fn binary_check(binary: &str, args: &[&str], version: Option<&str>) -> CheckResult {
    match Command::new(binary).args(args).output() {
        Ok(output) => {
            let observed = command_text(&output);
            let ok = output.status.success()
                && version.is_none_or(|fragment| observed.contains(fragment));
            let display = observed.lines().next().unwrap_or_default().to_owned();
            CheckResult {
                label: binary.to_owned(),
                ok,
                observed: if display.is_empty() {
                    format!("exit status {}", output.status)
                } else {
                    display
                },
                remedy: None,
            }
        }
        Err(error) => CheckResult {
            label: binary.to_owned(),
            ok: false,
            observed: error.to_string(),
            remedy: None,
        },
    }
}

fn tool_check(requirement: ToolRequirement) -> CheckResult {
    let mut result = binary_check(
        requirement.binary,
        requirement.args,
        Some(requirement.version_fragment),
    );
    result.label = requirement.label.to_owned();
    result.remedy = Some(requirement.install.to_owned());
    result
}

fn rust_toolchain_check(vm: Vm) -> CheckResult {
    let toolchain = vm.required_toolchain();
    let output = Command::new("rustup").args(["toolchain", "list"]).output();
    match output {
        Ok(output) => {
            let observed = command_text(&output);
            let mut ok =
                output.status.success() && observed.lines().any(|line| line.starts_with(toolchain));
            let mut detail = if ok { "installed" } else { "not installed" }.to_owned();
            if ok && vm == Vm::Openvm {
                let components = Command::new("rustup")
                    .args(["component", "list", "--toolchain", toolchain, "--installed"])
                    .output();
                let has_rust_src = components.is_ok_and(|output| {
                    output.status.success()
                        && String::from_utf8_lossy(&output.stdout)
                            .lines()
                            .any(|line| line.starts_with("rust-src"))
                });
                if !has_rust_src {
                    ok = false;
                    detail = "installed without rust-src".to_owned();
                }
            }
            CheckResult {
                label: format!("Rust toolchain {toolchain}"),
                ok,
                observed: detail,
                remedy: Some(vm.toolchain_install().to_owned()),
            }
        }
        Err(error) => CheckResult {
            label: format!("Rust toolchain {toolchain}"),
            ok: false,
            observed: error.to_string(),
            remedy: Some(vm.toolchain_install().to_owned()),
        },
    }
}

fn command_text(output: &ProcessOutput) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    match (stdout.is_empty(), stderr.is_empty()) {
        (false, true) => stdout,
        (true, false) => stderr,
        (false, false) => format!("{stdout} {stderr}"),
        (true, true) => String::new(),
    }
}

fn profile(args: ProfileArgs, repository: &Path) -> Result<()> {
    let started = Utc::now();
    let timer = Instant::now();
    let input = load_input(&args.input)?;
    let expected_output = guest_workload::run(input);
    let expected_digest = output_digest(&expected_output)?;
    let output_directory = args.out.unwrap_or_else(|| {
        repository
            .join("profiles")
            .join(started.format("%Y%m%d-%H%M%SZ").to_string())
    });
    let output_directory = if output_directory.is_absolute() {
        output_directory
    } else {
        repository.join(output_directory)
    };
    create_new_output_directory(&output_directory)?;
    let canonical_input = serde_json::to_vec_pretty(&input)?;
    fs::write(output_directory.join("input.json"), &canonical_input)?;

    let mut summaries = Vec::new();
    for vm in args.vm.members() {
        println!("Profiling {}...", vm.name());
        let adapter_directory = output_directory.join(vm.name());
        fs::create_dir(&adapter_directory)?;
        let mut summary = match profile_one(
            vm,
            repository,
            &output_directory.join("input.json"),
            &adapter_directory,
        ) {
            Ok(summary) => summary,
            Err(error) => AdapterSummary::failed(vm.name(), Vec::new(), format!("{error:#}")),
        };
        if !adapter_directory.join("stdout.log").exists() {
            fs::write(adapter_directory.join("stdout.log"), [])?;
        }
        if !adapter_directory.join("stderr.log").exists() {
            fs::write(
                adapter_directory.join("stderr.log"),
                summary.error.as_deref().unwrap_or_default(),
            )?;
        }
        summary
            .artifacts
            .insert("stdout".to_owned(), format!("{}/stdout.log", vm.name()));
        summary
            .artifacts
            .insert("stderr".to_owned(), format!("{}/stderr.log", vm.name()));
        summary
            .artifacts
            .insert("summary".to_owned(), format!("{}/summary.json", vm.name()));
        validate_adapter_output(
            &mut summary,
            &expected_output,
            &expected_digest,
            &output_directory,
        );
        let status = match summary.status {
            AdapterStatus::Success => "success",
            AdapterStatus::Failed => "failed",
            AdapterStatus::Skipped => "skipped",
        };
        println!("  {status}");
        write_json(&summary, &adapter_directory.join("summary.json"))?;
        summaries.push(summary);
    }

    let finished = Utc::now();
    let manifest = RunManifest {
        schema_version: SCHEMA_VERSION,
        started_at: started.to_rfc3339_opts(SecondsFormat::Secs, true),
        finished_at: finished.to_rfc3339_opts(SecondsFormat::Secs, true),
        duration_ms: timer.elapsed().as_millis() as u64,
        input_sha256: sha256(&canonical_input),
        expected_output,
        expected_output_digest: expected_digest,
        adapters: summaries,
    };
    write_json(&manifest, &output_directory.join("run.json"))?;
    fs::write(
        output_directory.join("summary.md"),
        render_summary(&manifest),
    )?;
    println!("Profile artifacts: {}", output_directory.display());
    if has_adapter_failure(&manifest.adapters) {
        bail!("one or more adapters failed; see run.json and the adapter logs");
    }
    Ok(())
}

fn load_input(path: &Path) -> Result<Input> {
    let bytes = fs::read(path).with_context(|| format!("read input file {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("parse input file {}", path.display()))
}

fn create_new_output_directory(path: &Path) -> Result<()> {
    if path.exists() {
        bail!("output directory already exists: {}", path.display());
    }
    fs::create_dir_all(path).with_context(|| format!("create {}", path.display()))
}

fn has_adapter_failure(adapters: &[AdapterSummary]) -> bool {
    adapters
        .iter()
        .any(|adapter| adapter.status != AdapterStatus::Success)
}

fn profile_one(vm: Vm, repository: &Path, input: &Path, out: &Path) -> Result<AdapterSummary> {
    let manifest = repository
        .join("adapters")
        .join(vm.name())
        .join("runner")
        .join("Cargo.toml");
    let command = vec![
        "cargo".to_owned(),
        "run".to_owned(),
        "--locked".to_owned(),
        "--release".to_owned(),
        "--manifest-path".to_owned(),
        manifest.display().to_string(),
        "--".to_owned(),
        "--input".to_owned(),
        input.display().to_string(),
        "--out".to_owned(),
        out.display().to_string(),
    ];
    for requirement in vm.required_tools() {
        let result = tool_check(*requirement);
        if !result.ok {
            return Ok(AdapterSummary::failed(
                vm.name(),
                command,
                format!(
                    "{} prerequisite is unavailable or has the wrong version: {}. Install with: {}",
                    requirement.label, result.observed, requirement.install
                ),
            ));
        }
    }
    let toolchain = rust_toolchain_check(vm);
    if !toolchain.ok {
        return Ok(AdapterSummary::failed(
            vm.name(),
            command,
            format!(
                "required Rust toolchain {} is missing; install it with: {}",
                vm.required_toolchain(),
                vm.toolchain_install()
            ),
        ));
    }
    let timer = Instant::now();
    let output = run_command(&command, repository)?;
    fs::write(out.join("stdout.log"), &output.stdout)?;
    fs::write(out.join("stderr.log"), &output.stderr)?;
    let summary_path = out.join("summary.json");
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr);
        let tail = detail
            .lines()
            .rev()
            .take(20)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n");
        let mut summary = AdapterSummary::failed(
            vm.name(),
            command,
            format!("adapter command failed with {}: {tail}", output.status),
        );
        summary.duration_ms = timer.elapsed().as_millis() as u64;
        return Ok(summary);
    }
    let mut summary: AdapterSummary = profile_schema::read_json(&summary_path)
        .with_context(|| format!("read {}", summary_path.display()))?;
    summary.duration_ms = timer.elapsed().as_millis() as u64;
    summary.commands.insert(0, command);
    summary
        .artifacts
        .insert("stdout".to_owned(), format!("{}/stdout.log", vm.name()));
    summary
        .artifacts
        .insert("stderr".to_owned(), format!("{}/stderr.log", vm.name()));
    Ok(summary)
}

fn run_command(command: &[String], current_directory: &Path) -> Result<ProcessOutput> {
    let (program, args) = command.split_first().context("empty command")?;
    Command::new(program)
        .args(args)
        .current_dir(current_directory)
        .output()
        .with_context(|| format!("run command: {}", display_command(command)))
}

fn display_command(command: &[String]) -> String {
    command
        .iter()
        .map(|part| {
            if part
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || "-._/:".contains(character))
            {
                part.clone()
            } else {
                format!("{part:?}")
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn validate_adapter_output(
    summary: &mut AdapterSummary,
    expected: &Output,
    digest: &str,
    output_directory: &Path,
) {
    if summary.status != AdapterStatus::Success {
        return;
    }
    if summary.output.as_ref() != Some(expected) {
        summary.status = AdapterStatus::Failed;
        summary.error = Some("public output does not match native execution".to_owned());
        return;
    }
    if summary.output_digest.as_deref() != Some(digest) {
        summary.status = AdapterStatus::Failed;
        summary.error = Some("normalized output digest does not match native execution".to_owned());
        return;
    }
    if summary
        .primary_metric
        .as_ref()
        .is_none_or(|metric| metric.value == 0)
    {
        summary.status = AdapterStatus::Failed;
        summary.error = Some("primary metric is missing or zero".to_owned());
        return;
    }
    if !summary
        .top_self
        .iter()
        .chain(&summary.top_inclusive)
        .any(|function| function.name.contains("guest_workload"))
    {
        summary.status = AdapterStatus::Failed;
        summary.error = Some("profile has no shared guest_workload function frames".to_owned());
        return;
    }
    if let Some(path) = summary
        .artifacts
        .iter()
        .filter(|(name, _)| name.as_str() != "summary")
        .map(|(_, path)| path)
        .find(|path| !output_directory.join(path).is_file())
    {
        summary.status = AdapterStatus::Failed;
        summary.error = Some(format!("declared artifact does not exist: {path}"));
    }
}

fn output_digest(output: &Output) -> Result<String> {
    Ok(sha256(&serde_json::to_vec(output)?))
}

fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn render_summary(manifest: &RunManifest) -> String {
    let mut markdown = String::from(
        "# zkVM profile summary\n\n\
         The metrics in this report use vendor-specific units. Do not compare metrics from different VMs. \
         OpenVM reports trace cells. SP1 reports cycles and prover gas. ZisK reports proof-area costs. \
         Do not combine these values into one score.\n\n\
         | VM | Status | Profiling mode | Primary metric | Output digest |\n\
         | --- | --- | --- | --- | --- |\n",
    );
    for adapter in &manifest.adapters {
        let status = match adapter.status {
            AdapterStatus::Success => "success",
            AdapterStatus::Failed => "failed",
            AdapterStatus::Skipped => "skipped",
        };
        let metric = adapter
            .primary_metric
            .as_ref()
            .map(|metric| format!("{} {} ({})", metric.value, metric.unit, metric.name))
            .unwrap_or_else(|| "n/a".to_owned());
        let digest = adapter.output_digest.as_deref().unwrap_or("n/a");
        markdown.push_str(&format!(
            "| {} | {status} | {} | {metric} | `{digest}` |\n",
            adapter.vm, adapter.profile_mode
        ));
    }
    for adapter in &manifest.adapters {
        markdown.push_str(&format!("\n## {}\n\n", adapter.vm));
        if let Some(error) = &adapter.error {
            markdown.push_str(&format!("Error: {error}\n\n"));
        }
        if !adapter.top_self.is_empty() {
            markdown.push_str("### Functions with the highest self cost\n\n| Function | Self | Inclusive |\n| --- | ---: | ---: |\n");
            for function in adapter.top_self.iter().take(10) {
                markdown.push_str(&format!(
                    "| `{}` | {} | {} |\n",
                    function.name.replace('|', "\\|"),
                    function.self_cost,
                    function.inclusive_cost
                ));
            }
        }
    }
    markdown
}

#[cfg(test)]
mod tests {
    use super::*;
    use profile_schema::Metric;
    use std::{collections::BTreeMap, ffi::OsString};

    fn success_summary(output: Output) -> AdapterSummary {
        AdapterSummary {
            vm: "fake".to_owned(),
            status: AdapterStatus::Success,
            sdk_version: "1".to_owned(),
            tool_version: "1".to_owned(),
            profile_mode: "test".to_owned(),
            duration_ms: 1,
            commands: Vec::new(),
            primary_metric: Some(Metric::new("cycles", "cycles", 1)),
            secondary_metrics: Vec::new(),
            output: Some(output),
            output_digest: Some(output_digest(&output).unwrap()),
            elf_sha256: None,
            top_self: Vec::new(),
            top_inclusive: Vec::new(),
            artifacts: BTreeMap::new(),
            error: None,
        }
    }

    #[test]
    fn detects_output_mismatches() {
        let expected = guest_workload::run(Input { seed: 1, rounds: 1 });
        let actual = guest_workload::run(Input { seed: 2, rounds: 1 });
        let digest = output_digest(&expected).unwrap();
        let mut summary = success_summary(actual);
        validate_adapter_output(&mut summary, &expected, &digest, Path::new("."));
        assert_eq!(summary.status, AdapterStatus::Failed);
    }

    #[test]
    fn command_capture_preserves_failure_status() {
        let directory = tempfile::tempdir().unwrap();
        let command = vec![
            OsString::from(if cfg!(windows) { "cmd" } else { "sh" })
                .to_string_lossy()
                .into_owned(),
            if cfg!(windows) { "/C" } else { "-c" }.to_owned(),
            if cfg!(windows) { "exit /b 7" } else { "exit 7" }.to_owned(),
        ];
        let output = run_command(&command, directory.path()).unwrap();
        assert_eq!(output.status.code(), Some(7));
    }

    #[test]
    fn fake_adapters_continue_after_one_fails() {
        let directory = tempfile::tempdir().unwrap();
        let shell = if cfg!(windows) { "cmd" } else { "sh" };
        let flag = if cfg!(windows) { "/C" } else { "-c" };
        let scripts = if cfg!(windows) {
            ["exit /b 7", "exit /b 0"]
        } else {
            ["exit 7", "exit 0"]
        };
        let statuses = scripts
            .into_iter()
            .map(|script| {
                run_command(
                    &[shell.to_owned(), flag.to_owned(), script.to_owned()],
                    directory.path(),
                )
                .unwrap()
                .status
                .code()
                .unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(statuses, [7, 0]);
    }

    #[test]
    fn summary_warns_that_metrics_are_not_comparable() {
        let output = guest_workload::run(Input { seed: 1, rounds: 1 });
        let manifest = RunManifest {
            schema_version: SCHEMA_VERSION,
            started_at: String::new(),
            finished_at: String::new(),
            duration_ms: 0,
            input_sha256: String::new(),
            expected_output: output,
            expected_output_digest: output_digest(&output).unwrap(),
            adapters: vec![success_summary(output)],
        };
        assert!(render_summary(&manifest).contains("Do not compare metrics from different VMs"));
    }

    #[test]
    fn missing_binary_is_reported() {
        let check = binary_check("a-binary-that-does-not-exist-zkevm-profile", &[], None);
        assert!(!check.ok);
    }

    #[test]
    fn malformed_input_is_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("input.json");
        fs::write(&path, b"{not json").unwrap();
        assert!(load_input(&path).is_err());
    }

    #[test]
    fn existing_output_directory_is_rejected() {
        let directory = tempfile::tempdir().unwrap();
        assert!(create_new_output_directory(directory.path()).is_err());
    }

    #[test]
    fn failure_status_keeps_all_adapter_results() {
        let output = guest_workload::run(Input { seed: 1, rounds: 1 });
        let adapters = vec![
            AdapterSummary::failed("first", vec!["fake".to_owned()], "failed"),
            success_summary(output),
        ];
        assert_eq!(adapters.len(), 2);
        assert!(has_adapter_failure(&adapters));
        assert_eq!(adapters[1].status, AdapterStatus::Success);
    }
}
