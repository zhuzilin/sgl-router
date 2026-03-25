use std::collections::BTreeMap;

use openmetrics_parser::{MetricsExposition, PrometheusType, PrometheusValue};
use tracing::warn;

#[derive(Debug)]
pub struct MetricPack {
    pub labels: Vec<(String, String)>,
    pub metrics_text: String,
}

type PrometheusExposition = MetricsExposition<PrometheusType, PrometheusValue>;

#[derive(Default)]
struct AggregatedFamilyText {
    metadata_lines: Vec<String>,
    sample_lines: Vec<String>,
}

/// Aggregate Prometheus metrics scraped from multiple sources into a unified one
pub fn aggregate_metrics(metric_packs: Vec<MetricPack>) -> anyhow::Result<String> {
    let mut families = BTreeMap::<String, AggregatedFamilyText>::new();
    for metric_pack in metric_packs {
        let metrics_text = &metric_pack.metrics_text;
        // openmetrics_parser doesn't handle colons in metric names; replace with underscores
        let metrics_text = metrics_text.replace(":", "_");

        let exposition = match openmetrics_parser::prometheus::parse_prometheus(&metrics_text) {
            Ok(x) => x,
            Err(err) => {
                warn!(
                    "aggregate_metrics error when parsing text: pack={:?} err={:?}",
                    metric_pack, err
                );
                continue;
            }
        };
        let exposition = transform_metrics(exposition, &metric_pack.labels);
        append_exposition_text(exposition, &mut families);
    }

    Ok(render_aggregated_families(families))
}

fn transform_metrics(
    mut exposition: PrometheusExposition,
    extra_labels: &[(String, String)],
) -> PrometheusExposition {
    for family in exposition.families.values_mut() {
        *family = family.with_labels(extra_labels.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    }
    exposition
}

fn append_exposition_text(
    exposition: PrometheusExposition,
    aggregated: &mut BTreeMap<String, AggregatedFamilyText>,
) {
    for (name, family) in exposition.families {
        let entry = aggregated.entry(name).or_default();
        for line in format!("{family}").lines().filter(|line| !line.is_empty()) {
            if line.starts_with("# ") {
                if !entry.metadata_lines.iter().any(|existing| existing == line) {
                    entry.metadata_lines.push(line.to_string());
                }
            } else {
                entry.sample_lines.push(line.to_string());
            }
        }
    }
}

fn render_aggregated_families(families: BTreeMap<String, AggregatedFamilyText>) -> String {
    let mut blocks = Vec::new();

    for family in families.into_values() {
        let mut lines = family.metadata_lines;
        lines.extend(family.sample_lines);
        if !lines.is_empty() {
            blocks.push(lines.join("\n"));
        }
    }

    if blocks.is_empty() {
        String::new()
    } else {
        format!("{}\n", blocks.join("\n\n"))
    }
}
