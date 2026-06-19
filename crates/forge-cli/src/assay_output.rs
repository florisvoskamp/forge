//! CI-friendly output renderers for `forge assay run`.
//!
//! Each renderer writes to stdout. The SARIF structs are defined locally so the crate does not
//! need a runtime SARIF dependency; they only need to be serialisable.

use forge_types::{AssayReport, FindingCategory, Severity};
use serde::Serialize;

// ---- human ----

/// Plain-text rendering (mirrors the TUI's assay panel tone).
pub fn print_human(report: &AssayReport) {
    let [crit, high, med, low] = report.severity_counts();
    println!(
        "assay · {} finding(s)  critical:{crit}  high:{high}  medium:{med}  low:{low}  cost:${:.4}  scope:{}",
        report.findings.len(),
        report.cost_usd,
        report.scope.label(),
    );
    if !report.skipped_lenses.is_empty() {
        for (lens, reason) in &report.skipped_lenses {
            println!("  skipped {lens}: {reason}");
        }
    }
    if report.findings.is_empty() {
        println!("  no findings — clean");
        return;
    }
    println!();
    for f in &report.findings {
        let loc = match f.line {
            Some(l) => format!("{}:{l}", f.file),
            None => f.file.clone(),
        };
        println!(
            "  [{sev}] [{lens}] {loc}",
            sev = f.severity.as_str(),
            lens = f.lens,
        );
        println!("    {}", f.title);
        if !f.rationale.is_empty() {
            println!("    why: {}", f.rationale);
        }
        if !f.suggested_fix.is_empty() {
            println!("    fix: {}", f.suggested_fix);
        }
        println!();
    }
}

// ---- markdown ----

/// PR-comment-friendly Markdown table.
pub fn print_markdown(report: &AssayReport) -> String {
    let [crit, high, med, low] = report.severity_counts();
    let mut out = String::new();
    out.push_str("## Forge Assay Report\n\n");
    out.push_str(&format!(
        "**Scope:** {}  **Cost:** ${:.4}  \
         **Findings:** {} (critical:{crit} high:{high} medium:{med} low:{low})\n\n",
        report.scope.label(),
        report.cost_usd,
        report.findings.len(),
    ));
    if !report.skipped_lenses.is_empty() {
        out.push_str("> **Skipped lenses:**");
        for (lens, reason) in &report.skipped_lenses {
            out.push_str(&format!(" {lens} ({reason});"));
        }
        out.push_str("\n\n");
    }
    if report.findings.is_empty() {
        out.push_str("**No findings — clean** ✓\n");
        return out;
    }
    out.push_str("| Severity | Location | Lens | Title |\n");
    out.push_str("|----------|----------|------|-------|\n");
    for f in &report.findings {
        let loc = match f.line {
            Some(l) => format!("{}:{l}", f.file),
            None => f.file.clone(),
        };
        out.push_str(&format!(
            "| {} | `{}` | {} | {} |\n",
            f.severity.as_str(),
            loc,
            f.lens,
            f.title.replace('|', "\\|"),
        ));
    }
    out
}

// ---- json ----

/// Emit `AssayReport` as pretty-printed JSON (AssayReport already derives Serialize).
pub fn print_json(report: &AssayReport) -> String {
    serde_json::to_string_pretty(report).expect("AssayReport is always serialisable")
}

// ---- SARIF 2.1.0 ----

/// Map Severity to a SARIF level string.
pub fn sarif_level(sev: Severity) -> &'static str {
    match sev {
        Severity::Critical | Severity::High => "error",
        Severity::Medium => "warning",
        Severity::Low => "note",
    }
}

// Minimal SARIF 2.1.0 structs (only the fields CI consumers care about).

#[derive(Serialize)]
struct SarifRoot {
    #[serde(rename = "$schema")]
    schema: &'static str,
    version: &'static str,
    runs: Vec<SarifRun>,
}

#[derive(Serialize)]
struct SarifRun {
    tool: SarifTool,
    rules: Vec<SarifRule>,
    results: Vec<SarifResult>,
}

#[derive(Serialize)]
struct SarifTool {
    driver: SarifDriver,
}

#[derive(Serialize)]
struct SarifDriver {
    name: &'static str,
    #[serde(rename = "informationUri")]
    information_uri: &'static str,
    rules: Vec<SarifRule>,
}

#[derive(Serialize, Clone)]
struct SarifRule {
    id: String,
    name: String,
    #[serde(rename = "shortDescription")]
    short_description: SarifText,
}

#[derive(Serialize, Clone)]
struct SarifText {
    text: String,
}

#[derive(Serialize)]
struct SarifResult {
    #[serde(rename = "ruleId")]
    rule_id: String,
    level: &'static str,
    message: SarifText,
    locations: Vec<SarifLocation>,
}

#[derive(Serialize)]
struct SarifLocation {
    #[serde(rename = "physicalLocation")]
    physical_location: SarifPhysicalLocation,
}

#[derive(Serialize)]
struct SarifPhysicalLocation {
    #[serde(rename = "artifactLocation")]
    artifact_location: SarifArtifactLocation,
    #[serde(skip_serializing_if = "Option::is_none")]
    region: Option<SarifRegion>,
}

#[derive(Serialize)]
struct SarifArtifactLocation {
    uri: String,
}

#[derive(Serialize)]
struct SarifRegion {
    #[serde(rename = "startLine")]
    start_line: u32,
}

/// Build a SARIF rule id from a `FindingCategory`.
fn rule_id(cat: FindingCategory) -> String {
    format!("forge-assay/{}", cat.as_str())
}

/// Emit a valid SARIF 2.1.0 document for the report.
pub fn print_sarif(report: &AssayReport) -> String {
    // Collect unique categories present in findings (preserve order).
    let mut seen_cats: Vec<FindingCategory> = Vec::new();
    for f in &report.findings {
        if !seen_cats.contains(&f.category) {
            seen_cats.push(f.category);
        }
    }

    let rules: Vec<SarifRule> = seen_cats
        .iter()
        .map(|&cat| SarifRule {
            id: rule_id(cat),
            name: cat.as_str().to_string(),
            short_description: SarifText {
                text: cat.as_str().to_string(),
            },
        })
        .collect();

    let results: Vec<SarifResult> = report
        .findings
        .iter()
        .map(|f| {
            let region = f.line.map(|l| SarifRegion { start_line: l });
            SarifResult {
                rule_id: rule_id(f.category),
                level: sarif_level(f.severity),
                message: SarifText {
                    text: format!("{} — {}", f.title, f.rationale),
                },
                locations: vec![SarifLocation {
                    physical_location: SarifPhysicalLocation {
                        artifact_location: SarifArtifactLocation {
                            uri: f.file.clone(),
                        },
                        region,
                    },
                }],
            }
        })
        .collect();

    let sarif = SarifRoot {
        schema: "https://schemastore.azurewebsites.net/schemas/json/sarif-2.1.0-rtm.5.json",
        version: "2.1.0",
        runs: vec![SarifRun {
            tool: SarifTool {
                driver: SarifDriver {
                    name: "forge-assay",
                    information_uri: "https://github.com/forge-ai/forge",
                    rules: rules.clone(),
                },
            },
            rules,
            results,
        }],
    };

    serde_json::to_string_pretty(&sarif).expect("SARIF is always serialisable")
}

// ---- tests ----

#[cfg(test)]
mod tests {
    use super::*;
    use forge_types::{AssayScope, Confidence, Effort, Finding, FindingCategory, Severity};

    fn make_finding(
        category: FindingCategory,
        severity: Severity,
        file: &str,
        line: Option<u32>,
        title: &str,
    ) -> Finding {
        Finding {
            id: "test-id".to_string(),
            category,
            severity,
            confidence: Confidence::High,
            file: file.to_string(),
            line,
            title: title.to_string(),
            rationale: "rationale".to_string(),
            suggested_fix: "fix it".to_string(),
            effort: Effort::Small,
            lens: category.as_str().to_string(),
            verified: true,
        }
    }

    fn fixture_report() -> AssayReport {
        AssayReport {
            run_id: "run-abc123".to_string(),
            scope: AssayScope::Diff,
            findings: vec![
                make_finding(
                    FindingCategory::Correctness,
                    Severity::Critical,
                    "src/main.rs",
                    Some(42),
                    "unwrap panics on error",
                ),
                make_finding(
                    FindingCategory::DeadWeight,
                    Severity::Low,
                    "src/lib.rs",
                    None,
                    "dead function",
                ),
            ],
            cost_usd: 0.0012,
            skipped_lenses: vec![],
        }
    }

    fn empty_report() -> AssayReport {
        AssayReport {
            run_id: "run-empty".to_string(),
            scope: AssayScope::Repo,
            findings: vec![],
            cost_usd: 0.0,
            skipped_lenses: vec![],
        }
    }

    // ---- sarif_level ----

    #[test]
    fn sarif_level_critical_is_error() {
        assert_eq!(sarif_level(Severity::Critical), "error");
    }

    #[test]
    fn sarif_level_high_is_error() {
        assert_eq!(sarif_level(Severity::High), "error");
    }

    #[test]
    fn sarif_level_medium_is_warning() {
        assert_eq!(sarif_level(Severity::Medium), "warning");
    }

    #[test]
    fn sarif_level_low_is_note() {
        assert_eq!(sarif_level(Severity::Low), "note");
    }

    // ---- print_json round-trip ----

    #[test]
    fn print_json_round_trips_through_serde() {
        let report = fixture_report();
        let json = print_json(&report);
        let parsed: AssayReport = serde_json::from_str(&json).expect("valid JSON");
        assert_eq!(parsed.run_id, report.run_id);
        assert_eq!(parsed.findings.len(), 2);
        assert_eq!(parsed.findings[0].severity, Severity::Critical);
        assert_eq!(parsed.findings[1].severity, Severity::Low);
    }

    #[test]
    fn print_json_empty_report_round_trips() {
        let report = empty_report();
        let json = print_json(&report);
        let parsed: AssayReport = serde_json::from_str(&json).expect("valid JSON");
        assert!(parsed.findings.is_empty());
    }

    // ---- print_sarif ----

    #[test]
    fn print_sarif_has_required_schema_and_version() {
        let sarif = print_sarif(&fixture_report());
        assert!(sarif.contains("2.1.0"), "SARIF version present: {sarif}");
        assert!(
            sarif.contains("schemastore.azurewebsites.net"),
            "$schema URI present: {sarif}"
        );
    }

    #[test]
    fn print_sarif_tool_driver_name_is_forge_assay() {
        let sarif = print_sarif(&fixture_report());
        assert!(
            sarif.contains("forge-assay"),
            "driver name present: {sarif}"
        );
    }

    #[test]
    fn print_sarif_result_count_matches_findings() {
        let report = fixture_report();
        let sarif = print_sarif(&report);
        let v: serde_json::Value = serde_json::from_str(&sarif).expect("valid JSON");
        let results = &v["runs"][0]["results"];
        assert_eq!(
            results.as_array().map(|a| a.len()).unwrap_or(0),
            report.findings.len(),
            "one SARIF result per finding"
        );
    }

    #[test]
    fn print_sarif_levels_map_correctly() {
        let report = fixture_report(); // critical + low
        let sarif = print_sarif(&report);
        let v: serde_json::Value = serde_json::from_str(&sarif).expect("valid JSON");
        let results = v["runs"][0]["results"].as_array().unwrap();
        // First finding is critical → "error"
        assert_eq!(results[0]["level"], "error");
        // Second is low → "note"
        assert_eq!(results[1]["level"], "note");
    }

    #[test]
    fn print_sarif_physical_location_and_region() {
        let report = fixture_report();
        let sarif = print_sarif(&report);
        let v: serde_json::Value = serde_json::from_str(&sarif).expect("valid JSON");
        let loc = &v["runs"][0]["results"][0]["locations"][0]["physicalLocation"];
        assert_eq!(loc["artifactLocation"]["uri"], "src/main.rs");
        assert_eq!(loc["region"]["startLine"], 42);
    }

    #[test]
    fn print_sarif_no_region_when_line_is_none() {
        let report = fixture_report();
        let sarif = print_sarif(&report);
        let v: serde_json::Value = serde_json::from_str(&sarif).expect("valid JSON");
        let loc = &v["runs"][0]["results"][1]["locations"][0]["physicalLocation"];
        assert!(loc["region"].is_null(), "no region when line is None");
    }

    #[test]
    fn print_sarif_empty_report_has_zero_results() {
        let sarif = print_sarif(&empty_report());
        let v: serde_json::Value = serde_json::from_str(&sarif).expect("valid JSON");
        let results = &v["runs"][0]["results"];
        assert_eq!(results.as_array().map(|a| a.len()).unwrap_or(0), 0);
    }

    // ---- print_markdown ----

    #[test]
    fn print_markdown_contains_a_row_per_finding() {
        let md = print_markdown(&fixture_report());
        // Two findings → two table rows (plus header + separator = 4 lines with `|`)
        let rows: Vec<&str> = md.lines().filter(|l| l.starts_with('|')).collect();
        // header + separator + 2 data rows
        assert_eq!(rows.len(), 4, "header + sep + 2 rows: {md}");
    }

    #[test]
    fn print_markdown_clean_message_on_empty_report() {
        let md = print_markdown(&empty_report());
        assert!(
            md.contains("No findings") || md.contains("clean"),
            "clean message present: {md}"
        );
        assert!(
            !md.contains("| critical |") && !md.contains("| low |"),
            "no table rows in clean output: {md}"
        );
    }

    #[test]
    fn print_markdown_finding_titles_appear_in_output() {
        let md = print_markdown(&fixture_report());
        assert!(md.contains("unwrap panics on error"), "first title: {md}");
        assert!(md.contains("dead function"), "second title: {md}");
    }
}
