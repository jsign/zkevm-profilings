use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::{BufReader, BufWriter, Read, Write},
    path::Path,
};

use anyhow::{anyhow, bail, Context, Result};
use flate2::read::GzDecoder;
use guest_workload::Output;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum AdapterStatus {
    Success,
    Failed,
    Skipped,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Metric {
    pub name: String,
    pub unit: String,
    pub value: u64,
}

impl Metric {
    pub fn new(name: impl Into<String>, unit: impl Into<String>, value: u64) -> Self {
        Self {
            name: name.into(),
            unit: unit.into(),
            value,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FunctionCost {
    pub name: String,
    pub self_cost: u64,
    pub inclusive_cost: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AdapterSummary {
    pub vm: String,
    pub status: AdapterStatus,
    pub sdk_version: String,
    pub tool_version: String,
    pub profile_mode: String,
    pub duration_ms: u64,
    pub commands: Vec<Vec<String>>,
    pub primary_metric: Option<Metric>,
    #[serde(default)]
    pub secondary_metrics: Vec<Metric>,
    pub output: Option<Output>,
    pub output_digest: Option<String>,
    pub elf_sha256: Option<String>,
    #[serde(default)]
    pub top_self: Vec<FunctionCost>,
    #[serde(default)]
    pub top_inclusive: Vec<FunctionCost>,
    #[serde(default)]
    pub artifacts: BTreeMap<String, String>,
    pub error: Option<String>,
}

impl AdapterSummary {
    pub fn failed(vm: &str, command: Vec<String>, error: impl Into<String>) -> Self {
        Self {
            vm: vm.to_owned(),
            status: AdapterStatus::Failed,
            sdk_version: String::new(),
            tool_version: String::new(),
            profile_mode: "unavailable".to_owned(),
            duration_ms: 0,
            commands: if command.is_empty() {
                Vec::new()
            } else {
                vec![command]
            },
            primary_metric: None,
            secondary_metrics: Vec::new(),
            output: None,
            output_digest: None,
            elf_sha256: None,
            top_self: Vec::new(),
            top_inclusive: Vec::new(),
            artifacts: BTreeMap::new(),
            error: Some(error.into()),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RunManifest {
    pub schema_version: u32,
    pub started_at: String,
    pub finished_at: String,
    pub duration_ms: u64,
    pub input_sha256: String,
    pub expected_output: Output,
    pub expected_output_digest: String,
    pub adapters: Vec<AdapterSummary>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ParsedProfile {
    pub folded: BTreeMap<String, u64>,
    pub functions: Vec<FunctionCost>,
}

impl ParsedProfile {
    pub fn top_self(&self, count: usize) -> Vec<FunctionCost> {
        let mut values = self.functions.clone();
        values.sort_by(|a, b| {
            b.self_cost
                .cmp(&a.self_cost)
                .then_with(|| a.name.cmp(&b.name))
        });
        values.truncate(count);
        values
    }

    pub fn top_inclusive(&self, count: usize) -> Vec<FunctionCost> {
        let mut values = self.functions.clone();
        values.sort_by(|a, b| {
            b.inclusive_cost
                .cmp(&a.inclusive_cost)
                .then_with(|| a.name.cmp(&b.name))
        });
        values.truncate(count);
        values
    }
}

fn table_index(table: &Value, column: &str) -> Result<usize> {
    table
        .get("schema")
        .and_then(|schema| schema.get(column))
        .and_then(Value::as_u64)
        .map(|index| index as usize)
        .ok_or_else(|| anyhow!("profile table has no {column} column"))
}

fn optional_table_index(table: &Value, column: &str) -> Option<usize> {
    table
        .get("schema")?
        .get(column)?
        .as_u64()
        .map(|index| index as usize)
}

fn row_value(row: &Value, index: usize) -> Option<&Value> {
    row.as_array()?.get(index)
}

fn clean_frame(name: &str) -> String {
    let name = name.trim();
    let name = if name.is_empty() { "[unknown]" } else { name };
    name.replace(';', ":").replace('\n', " ")
}

fn firefox_threads(value: &Value) -> Vec<&Value> {
    let mut threads = value
        .get("threads")
        .and_then(Value::as_array)
        .map(|threads| threads.iter().collect::<Vec<_>>())
        .unwrap_or_default();
    if let Some(processes) = value.get("processes").and_then(Value::as_array) {
        for process in processes {
            threads.extend(firefox_threads(process));
        }
    }
    if let Some(profiles) = value.get("profiles").and_then(Value::as_array) {
        for profile in profiles {
            threads.extend(firefox_threads(profile));
        }
    }
    threads
}

/// Parse a Firefox Profiler JSON document to get folded stacks and function totals.
pub fn parse_firefox_profile(value: &Value, default_sample_weight: u64) -> Result<ParsedProfile> {
    if value.get("shared").is_some() {
        return parse_preprocessed_firefox_profile(value, default_sample_weight);
    }
    let threads = firefox_threads(value);
    if threads.is_empty() {
        bail!("Firefox profile has no threads");
    }

    let mut folded = BTreeMap::<String, u64>::new();
    let mut self_costs = BTreeMap::<String, u64>::new();
    let mut inclusive_costs = BTreeMap::<String, u64>::new();

    for thread in threads {
        let strings = thread
            .get("stringTable")
            .and_then(Value::as_array)
            .context("Firefox thread has no stringTable")?;
        let frames = thread
            .get("frameTable")
            .context("Firefox thread has no frameTable")?;
        let stacks = thread
            .get("stackTable")
            .context("Firefox thread has no stackTable")?;
        let samples = thread
            .get("samples")
            .context("Firefox thread has no samples")?;

        let frame_location = table_index(frames, "location")?;
        let stack_frame = table_index(stacks, "frame")?;
        let stack_prefix = optional_table_index(stacks, "prefix");
        let sample_stack = table_index(samples, "stack")?;
        let sample_weight = optional_table_index(samples, "weight");
        let frame_rows = frames
            .get("data")
            .and_then(Value::as_array)
            .context("frameTable has no data")?;
        let stack_rows = stacks
            .get("data")
            .and_then(Value::as_array)
            .context("stackTable has no data")?;
        let sample_rows = samples
            .get("data")
            .and_then(Value::as_array)
            .context("samples has no data")?;

        for sample in sample_rows {
            let Some(mut stack_index) = row_value(sample, sample_stack).and_then(Value::as_u64)
            else {
                continue;
            };
            let weight = sample_weight
                .and_then(|index| row_value(sample, index))
                .and_then(Value::as_f64)
                .map(|weight| weight.round().max(1.0) as u64)
                .unwrap_or(default_sample_weight.max(1));
            let mut path = Vec::<String>::new();
            loop {
                let stack_row = stack_rows
                    .get(stack_index as usize)
                    .context("stack index is outside stackTable")?;
                let frame_index = row_value(stack_row, stack_frame)
                    .and_then(Value::as_u64)
                    .context("stack row has no frame")? as usize;
                let frame_row = frame_rows
                    .get(frame_index)
                    .context("frame index is outside frameTable")?;
                let string_index = row_value(frame_row, frame_location)
                    .and_then(Value::as_u64)
                    .context("frame row has no location")?
                    as usize;
                let name = strings
                    .get(string_index)
                    .and_then(Value::as_str)
                    .context("frame location is outside stringTable")?;
                path.push(clean_frame(name));
                let Some(prefix) = stack_prefix
                    .and_then(|index| row_value(stack_row, index))
                    .and_then(Value::as_u64)
                else {
                    break;
                };
                stack_index = prefix;
            }
            path.reverse();
            if path.is_empty() {
                continue;
            }
            *folded.entry(path.join(";")).or_default() += weight;
            *self_costs.entry(path[path.len() - 1].clone()).or_default() += weight;
            let mut seen = BTreeSet::new();
            for frame in path {
                if seen.insert(frame.clone()) {
                    *inclusive_costs.entry(frame).or_default() += weight;
                }
            }
        }
    }
    Ok(finish_profile(folded, self_costs, inclusive_costs))
}

fn parse_preprocessed_firefox_profile(
    value: &Value,
    default_sample_weight: u64,
) -> Result<ParsedProfile> {
    let shared = value
        .get("shared")
        .context("profile has no shared tables")?;
    let strings = shared
        .get("stringArray")
        .and_then(Value::as_array)
        .context("shared table has no stringArray")?;
    let frame_functions = shared
        .pointer("/frameTable/func")
        .and_then(Value::as_array)
        .context("shared frameTable has no func column")?;
    let function_names = shared
        .pointer("/funcTable/name")
        .and_then(Value::as_array)
        .context("shared funcTable has no name column")?;
    let stack_frames = shared
        .pointer("/stackTable/frame")
        .and_then(Value::as_array)
        .context("shared stackTable has no frame column")?;
    let stack_prefixes = shared
        .pointer("/stackTable/prefix")
        .and_then(Value::as_array)
        .context("shared stackTable has no prefix column")?;
    let threads = firefox_threads(value);
    if threads.is_empty() {
        bail!("Firefox profile has no threads");
    }
    let mut folded = BTreeMap::<String, u64>::new();
    let mut self_costs = BTreeMap::<String, u64>::new();
    let mut inclusive_costs = BTreeMap::<String, u64>::new();
    for thread in threads {
        let sample_stacks = thread
            .pointer("/samples/stack")
            .and_then(Value::as_array)
            .context("thread samples have no stack column")?;
        let sample_weights = thread.pointer("/samples/weight").and_then(Value::as_array);
        let weight_scale = if thread
            .pointer("/samples/weightType")
            .and_then(Value::as_str)
            == Some("tracing-ms")
        {
            1_000.0
        } else {
            1.0
        };
        for (sample_index, stack) in sample_stacks.iter().enumerate() {
            let Some(mut stack_index) = stack.as_u64() else {
                continue;
            };
            let weight = sample_weights
                .and_then(|weights| weights.get(sample_index))
                .and_then(Value::as_f64)
                .map(|weight| (weight * weight_scale).round().max(1.0) as u64)
                .unwrap_or(default_sample_weight.max(1));
            let mut path = Vec::new();
            loop {
                let frame_index = stack_frames
                    .get(stack_index as usize)
                    .and_then(Value::as_u64)
                    .context("stack frame is outside frameTable")?
                    as usize;
                let function_index = frame_functions
                    .get(frame_index)
                    .and_then(Value::as_u64)
                    .context("frame function is outside funcTable")?
                    as usize;
                let string_index = function_names
                    .get(function_index)
                    .and_then(Value::as_u64)
                    .context("function name is outside stringArray")?
                    as usize;
                let name = strings
                    .get(string_index)
                    .and_then(Value::as_str)
                    .context("function name is not a string")?;
                path.push(clean_frame(name));
                let Some(prefix) = stack_prefixes
                    .get(stack_index as usize)
                    .and_then(Value::as_u64)
                else {
                    break;
                };
                stack_index = prefix;
            }
            path.reverse();
            add_stack_cost(
                &path,
                weight,
                &mut folded,
                &mut self_costs,
                &mut inclusive_costs,
            );
        }
    }
    Ok(finish_profile(folded, self_costs, inclusive_costs))
}

fn add_stack_cost(
    path: &[String],
    cost: u64,
    folded: &mut BTreeMap<String, u64>,
    self_costs: &mut BTreeMap<String, u64>,
    inclusive_costs: &mut BTreeMap<String, u64>,
) {
    if path.is_empty() {
        return;
    }
    *folded.entry(path.join(";")).or_default() += cost;
    *self_costs.entry(path[path.len() - 1].clone()).or_default() += cost;
    let mut seen = BTreeSet::new();
    for frame in path {
        if seen.insert(frame) {
            *inclusive_costs.entry(frame.clone()).or_default() += cost;
        }
    }
}

pub fn read_firefox_profile(path: &Path, default_sample_weight: u64) -> Result<ParsedProfile> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut reader: Box<dyn Read> = if path.extension().is_some_and(|extension| extension == "gz") {
        Box::new(GzDecoder::new(file))
    } else {
        Box::new(file)
    };
    let value: Value = serde_json::from_reader(&mut reader)
        .with_context(|| format!("parse Firefox profile {}", path.display()))?;
    parse_firefox_profile(&value, default_sample_weight)
}

fn find_metric_entries<'a>(value: &'a Value, entries: &mut Vec<&'a Value>) {
    match value {
        Value::Array(values) => {
            for value in values {
                find_metric_entries(value, entries);
            }
        }
        Value::Object(object) => {
            if object.contains_key("metric") && object.contains_key("value") {
                entries.push(value);
            } else {
                for value in object.values() {
                    find_metric_entries(value, entries);
                }
            }
        }
        _ => {}
    }
}

fn metric_value(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_f64().map(|number| number.round().max(0.0) as u64))
        .or_else(|| value.as_str()?.parse().ok())
}

fn openvm_symbol(symbols: &[u8], offset: usize) -> Result<String> {
    let suffix = symbols
        .get(offset..)
        .with_context(|| format!("OpenVM symbol offset {offset} is outside the symbol table"))?;
    let end = suffix
        .iter()
        .position(|byte| *byte == 0)
        .with_context(|| format!("OpenVM symbol at offset {offset} is not NUL-terminated"))?;
    let name = std::str::from_utf8(&suffix[..end])
        .with_context(|| format!("OpenVM symbol at offset {offset} is not UTF-8"))?;
    Ok(clean_frame(name))
}

fn openvm_frame(name: &str, symbols: Option<&[u8]>) -> Result<Option<String>> {
    let name = name.trim();
    if name.is_empty() {
        return Ok(None);
    }
    if let (Some(symbols), Ok(offset)) = (symbols, name.parse::<usize>()) {
        return openvm_symbol(symbols, offset).map(Some);
    }
    Ok(Some(clean_frame(name)))
}

fn metric_stack(entry: &Value, symbols: Option<&[u8]>) -> Result<Vec<String>> {
    let labels = entry.get("labels").unwrap_or(&Value::Null);
    let array_label = |name: &str| {
        labels.as_array()?.iter().find_map(|pair| {
            let pair = pair.as_array()?;
            (pair.first()?.as_str()? == name)
                .then(|| pair.get(1))
                .flatten()
        })
    };
    let value = labels
        .get("cycle_tracker_span")
        .or_else(|| labels.get("function"))
        .or_else(|| array_label("cycle_tracker_span"))
        .or_else(|| array_label("function"))
        .or_else(|| entry.get("cycle_tracker_span"));
    let names: Vec<&str> = match value {
        Some(Value::Array(values)) => values.iter().filter_map(Value::as_str).collect(),
        Some(Value::String(value)) => value.split([';', '>']).collect(),
        _ => return Ok(Vec::new()),
    };
    names
        .into_iter()
        .filter_map(|name| openvm_frame(name, symbols).transpose())
        .collect()
}

/// Parse OpenVM perf-metrics JSON for one metric, such as `cells_used`.
pub fn parse_openvm_metrics(value: &Value, metric_name: &str) -> Result<ParsedProfile> {
    parse_openvm_metrics_inner(value, metric_name, None)
}

/// Parse OpenVM perf-metrics and resolve numeric function spans through the guest symbol table.
pub fn parse_openvm_metrics_with_symbols(
    value: &Value,
    metric_name: &str,
    symbols: &[u8],
) -> Result<ParsedProfile> {
    parse_openvm_metrics_inner(value, metric_name, Some(symbols))
}

fn parse_openvm_metrics_inner(
    value: &Value,
    metric_name: &str,
    symbols: Option<&[u8]>,
) -> Result<ParsedProfile> {
    let mut entries = Vec::new();
    find_metric_entries(value, &mut entries);
    let mut folded = BTreeMap::<String, u64>::new();
    let mut self_costs = BTreeMap::<String, u64>::new();
    let mut inclusive_costs = BTreeMap::<String, u64>::new();
    for entry in entries {
        if entry.get("metric").and_then(Value::as_str) != Some(metric_name) {
            continue;
        }
        let Some(cost) = entry.get("value").and_then(metric_value) else {
            continue;
        };
        let mut path = metric_stack(entry, symbols)?;
        if path.is_empty() {
            path.push("[unattributed]".to_owned());
        }
        *folded.entry(path.join(";")).or_default() += cost;
        *self_costs.entry(path[path.len() - 1].clone()).or_default() += cost;
        let mut seen = BTreeSet::new();
        for frame in path {
            if seen.insert(frame.clone()) {
                *inclusive_costs.entry(frame).or_default() += cost;
            }
        }
    }
    if folded.is_empty() {
        bail!("OpenVM profile contains no {metric_name} metrics");
    }
    Ok(finish_profile(folded, self_costs, inclusive_costs))
}

fn finish_profile(
    folded: BTreeMap<String, u64>,
    self_costs: BTreeMap<String, u64>,
    inclusive_costs: BTreeMap<String, u64>,
) -> ParsedProfile {
    let names: BTreeSet<_> = self_costs
        .keys()
        .chain(inclusive_costs.keys())
        .cloned()
        .collect();
    let functions = names
        .into_iter()
        .map(|name| FunctionCost {
            self_cost: self_costs.get(&name).copied().unwrap_or(0),
            inclusive_cost: inclusive_costs.get(&name).copied().unwrap_or(0),
            name,
        })
        .collect();
    ParsedProfile { folded, functions }
}

pub fn write_folded(profile: &ParsedProfile, path: &Path) -> Result<()> {
    let mut writer =
        BufWriter::new(File::create(path).with_context(|| format!("create {}", path.display()))?);
    for (stack, cost) in &profile.folded {
        writeln!(writer, "{stack} {cost}")?;
    }
    Ok(())
}

pub fn write_flamegraph(
    profile: &ParsedProfile,
    path: &Path,
    title: &str,
    count_name: &str,
) -> Result<()> {
    let mut options = inferno::flamegraph::Options::default();
    options.title = title.to_owned();
    options.count_name = count_name.to_owned();
    let lines: Vec<String> = profile
        .folded
        .iter()
        .map(|(stack, cost)| format!("{stack} {cost}"))
        .collect();
    let writer =
        BufWriter::new(File::create(path).with_context(|| format!("create {}", path.display()))?);
    inferno::flamegraph::from_lines(&mut options, lines.iter().map(String::as_str), writer)
        .context("generate flamegraph")
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ZiskStatistics {
    pub steps: Option<u64>,
    pub base_cost: Option<u64>,
    pub variable_cost: Option<u64>,
    pub total_cost: Option<u64>,
}

/// Get stable top-level values from the readable ZisK statistics output.
pub fn parse_zisk_statistics(text: &str) -> ZiskStatistics {
    let mut statistics = ZiskStatistics::default();
    for line in text.lines() {
        let cleaned = line.trim_start_matches(|character: char| !character.is_ascii_alphabetic());
        let upper = cleaned.to_ascii_uppercase();
        let number = cleaned
            .split(|character: char| !character.is_ascii_digit())
            .find(|part| !part.is_empty())
            .and_then(|part| part.parse::<u64>().ok());
        if upper.starts_with("STEPS") {
            statistics.steps = number;
        } else if upper.starts_with("BASE") {
            statistics.base_cost = number;
        } else if upper.starts_with("VARIABLE") {
            statistics.variable_cost = number;
        } else if upper.starts_with("TOTAL") {
            statistics.total_cost = number;
        }
    }
    statistics
}

/// Parse cumulative function costs from the ZisK SDK-style `TOP COST FUNCTIONS` table.
pub fn parse_zisk_function_costs(text: &str) -> Vec<FunctionCost> {
    let mut in_table = false;
    let mut functions = Vec::new();
    for line in text.lines() {
        if line.contains("TOP COST FUNCTIONS") {
            in_table = true;
            continue;
        }
        if in_table && line.trim_start().starts_with('╚') {
            break;
        }
        if !in_table {
            continue;
        }
        let Some(bar_index) = line.find(['█', '░']) else {
            continue;
        };
        let label = line[..bar_index]
            .trim_matches(|character: char| character == '║' || character.is_whitespace());
        let name = label
            .split_once(' ')
            .map(|(_, name)| name.trim())
            .unwrap_or(label);
        if name.is_empty() {
            continue;
        }
        let cost = line[bar_index..]
            .split(|character: char| !character.is_ascii_digit())
            .find(|part| !part.is_empty())
            .and_then(|part| part.parse::<u64>().ok());
        if let Some(cost) = cost {
            functions.push(FunctionCost {
                name: clean_frame(name),
                self_cost: 0,
                inclusive_cost: cost,
            });
        }
    }
    functions
}

pub fn write_zisk_stats_csv(statistics: &ZiskStatistics, path: &Path) -> Result<()> {
    let mut writer = BufWriter::new(File::create(path)?);
    writeln!(writer, "metric,value")?;
    for (name, value) in [
        ("steps", statistics.steps),
        ("base_cost", statistics.base_cost),
        ("variable_cost", statistics.variable_cost),
        ("total_cost", statistics.total_cost),
    ] {
        if let Some(value) = value {
            writeln!(writer, "{name},{value}")?;
        }
    }
    Ok(())
}

pub fn write_zisk_html(
    statistics: &ZiskStatistics,
    functions: &[FunctionCost],
    path: &Path,
) -> Result<()> {
    let mut writer = BufWriter::new(File::create(path)?);
    writeln!(
        writer,
        "<!doctype html><meta charset=\"utf-8\"><title>ZisK profile</title>\
         <style>body{{font:14px system-ui;margin:2rem;max-width:70rem}}\
         table{{border-collapse:collapse;width:100%}}th,td{{border:1px solid #ccc;padding:.4rem;text-align:left}}\
         td:nth-child(n+2){{text-align:right}}</style><h1>ZisK profile</h1>"
    )?;
    writeln!(
        writer,
        "<h2>Statistics</h2><table><tr><th>Metric</th><th>Value</th></tr>"
    )?;
    for (name, value) in [
        ("Steps", statistics.steps),
        ("Base cost", statistics.base_cost),
        ("Variable cost", statistics.variable_cost),
        ("Total cost", statistics.total_cost),
    ] {
        if let Some(value) = value {
            writeln!(writer, "<tr><td>{name}</td><td>{value}</td></tr>")?;
        }
    }
    writeln!(writer, "</table><h2>Functions</h2><table><tr><th>Function</th><th>Self</th><th>Inclusive</th></tr>")?;
    for function in functions {
        writeln!(
            writer,
            "<tr><td>{}</td><td>{}</td><td>{}</td></tr>",
            html_escape(&function.name),
            function.self_cost,
            function.inclusive_cost
        )?;
    }
    writeln!(writer, "</table>")?;
    Ok(())
}

fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

pub fn write_json<T: Serialize>(value: &T, path: &Path) -> Result<()> {
    let writer = BufWriter::new(File::create(path)?);
    serde_json::to_writer_pretty(writer, value)?;
    Ok(())
}

pub fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let reader = BufReader::new(File::open(path)?);
    Ok(serde_json::from_reader(reader)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{write::GzEncoder, Compression};

    fn firefox_fixture() -> Value {
        serde_json::from_str(include_str!(
            "../../../fixtures/profiles/firefox-gecko.json"
        ))
        .unwrap()
    }

    #[test]
    fn reconstructs_firefox_stacks_and_costs() {
        let profile = parse_firefox_profile(&firefox_fixture(), 1).unwrap();
        assert_eq!(
            profile.folded["root;guest_workload::run;guest_workload::mix"],
            7
        );
        let run = profile
            .functions
            .iter()
            .find(|item| item.name == "guest_workload::run")
            .unwrap();
        assert_eq!((run.self_cost, run.inclusive_cost), (3, 10));
    }

    #[test]
    fn reads_gzip_profiles_and_generates_svg() {
        let directory = tempfile::tempdir().unwrap();
        let gzip_path = directory.path().join("profile.json.gz");
        let file = File::create(&gzip_path).unwrap();
        let mut encoder = GzEncoder::new(file, Compression::default());
        serde_json::to_writer(&mut encoder, &firefox_fixture()).unwrap();
        encoder.finish().unwrap();
        let profile = read_firefox_profile(&gzip_path, 1).unwrap();
        let svg_path = directory.path().join("flamegraph.svg");
        write_flamegraph(&profile, &svg_path, "Fixture", "cycles").unwrap();
        assert!(std::fs::read_to_string(svg_path).unwrap().contains("<svg"));
    }

    #[test]
    fn parses_openvm_metrics() {
        let value: Value = serde_json::from_str(include_str!(
            "../../../fixtures/profiles/openvm-metrics.json"
        ))
        .unwrap();
        let profile = parse_openvm_metrics(&value, "cells_used").unwrap();
        assert_eq!(
            profile.folded["guest_workload::run;guest_workload::mix"],
            12
        );
        assert_eq!(
            profile.folded["guest_workload::run;guest_workload::finalize"],
            8
        );
    }

    #[test]
    fn resolves_openvm_function_span_offsets() {
        let symbols = b"\0root\0guest_workload::run\0guest_workload::mix\0";
        let offset = |name: &[u8]| {
            symbols
                .windows(name.len())
                .position(|window| window == name)
                .unwrap()
        };
        let run = offset(b"guest_workload::run");
        let mix = offset(b"guest_workload::mix");
        let value = serde_json::json!({
            "counter": [{
                "metric": "cells_used",
                "labels": [["cycle_tracker_span", format!("{run};{mix}")]],
                "value": "12"
            }]
        });

        let profile = parse_openvm_metrics_with_symbols(&value, "cells_used", symbols).unwrap();
        assert_eq!(
            profile.folded["guest_workload::run;guest_workload::mix"],
            12
        );
    }

    #[test]
    fn treats_an_empty_openvm_span_as_unattributed() {
        let value = serde_json::json!({
            "counter": [{
                "metric": "cells_used",
                "labels": [["cycle_tracker_span", ""]],
                "value": "7"
            }]
        });

        let profile = parse_openvm_metrics(&value, "cells_used").unwrap();
        assert_eq!(profile.folded["[unattributed]"], 7);
    }

    #[test]
    fn aggregates_recursive_frames_once_for_inclusive_cost() {
        let value = serde_json::json!({
            "threads": [{
                "stringTable": ["recurse"],
                "frameTable": {"schema": {"location": 0}, "data": [[0]]},
                "stackTable": {"schema": {"prefix": 0, "frame": 1}, "data": [[null, 0], [0, 0]]},
                "samples": {"schema": {"stack": 0}, "data": [[1]]}
            }]
        });
        let profile = parse_firefox_profile(&value, 5).unwrap();
        assert_eq!(profile.functions[0].inclusive_cost, 5);
    }

    #[test]
    fn rejects_malformed_profiles() {
        assert!(parse_firefox_profile(&serde_json::json!({}), 1).is_err());
    }

    #[test]
    fn parses_zisk_statistics() {
        let statistics =
            parse_zisk_statistics(include_str!("../../../fixtures/profiles/zisk-stats.txt"));
        assert_eq!(statistics.steps, Some(92_875_129));
        assert_eq!(statistics.total_cost, Some(11_437_643_381));
        let functions =
            parse_zisk_function_costs(include_str!("../../../fixtures/profiles/zisk-stats.txt"));
        assert_eq!(functions[0].name, "guest_workload::run");
        assert_eq!(functions[0].inclusive_cost, 11_144_042_101);
    }

    #[test]
    fn parses_zisk_preprocessed_firefox_profile() {
        let value: Value =
            serde_json::from_str(include_str!("../../../fixtures/profiles/zisk-firefox.json"))
                .unwrap();
        let profile = parse_firefox_profile(&value, 1).unwrap();
        assert_eq!(
            profile.folded["guest_workload::run;guest_workload::mix"],
            5_000
        );
        let run = profile
            .functions
            .iter()
            .find(|function| function.name == "guest_workload::run")
            .unwrap();
        assert_eq!((run.self_cost, run.inclusive_cost), (3_000, 8_000));
    }
}
