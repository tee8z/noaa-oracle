//! Offline audit of saved AWC XML. No network requests or source mutations.
//! cargo run -p daemon --example audit_metars -- input.xml > audit.json
use std::{collections::BTreeMap, io::Write};

use anyhow::Context;
use daemon::{CurrentWeather, ObservationData, parse_xml};
use serde_json::json;
use sha2::{Digest, Sha256};

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args_os().skip(1);
    let path = args
        .next()
        .context("usage: audit_metars <saved-xml-file>")?;
    anyhow::ensure!(args.next().is_none(), "expected one saved XML file");
    let source = std::fs::read_to_string(&path)?;
    let hash: String = Sha256::digest(source.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let data: ObservationData = parse_xml(&source)?;
    let mut counts = BTreeMap::<String, usize>::new();
    let mut issues = Vec::new();
    for report in &data.data.metar {
        let (status, reason) = match CurrentWeather::try_from(report.clone()) {
            Ok(weather) => (weather.quality_status, weather.quality_reason),
            Err(error) => ("unrepresentable".into(), Some(error.to_string())),
        };
        *counts.entry(status.clone()).or_default() += 1;
        if status != "validated" {
            issues.push(json!({ "status": status, "reason": reason, "report": report }));
        }
    }
    let output = json!({
        "source_file": path.to_string_lossy(),
        "source_sha256": hash,
        "scope": "all source reports; no station-catalog filtering or temporal/seasonal checks",
        "reports": data.data.metar.len(),
        "counts": counts,
        "issues": issues,
    });
    let mut stdout = std::io::stdout().lock();
    serde_json::to_writer_pretty(&mut stdout, &output)?;
    writeln!(&mut stdout)?;
    Ok(())
}
