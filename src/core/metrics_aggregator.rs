use std::collections::{hash_map::Entry, BTreeMap};

use openmetrics_parser::{
    MetricFamily, MetricNumber, MetricsExposition, PrometheusType, PrometheusValue, Sample,
};
use tracing::warn;

#[derive(Debug)]
pub struct MetricPack {
    pub labels: Vec<(String, String)>,
    pub metrics_text: String,
}

type PrometheusExposition = MetricsExposition<PrometheusType, PrometheusValue>;

const HISTOGRAM_QUANTILES: [(&str, f64); 3] = [("p50", 0.50), ("p75", 0.75), ("p99", 0.99)];

#[derive(Default)]
struct AggregatedFamilyText {
    metadata_lines: Vec<String>,
    sample_lines: Vec<String>,
}

#[derive(Default)]
struct MetricFamilyBlock {
    name: Option<String>,
    lines: Vec<String>,
}

/// Aggregate Prometheus metrics scraped from multiple sources into a unified one.
pub fn aggregate_metrics(metric_packs: Vec<MetricPack>) -> anyhow::Result<String> {
    let mut families = BTreeMap::<String, AggregatedFamilyText>::new();
    for metric_pack in metric_packs {
        let metrics_text = &metric_pack.metrics_text;
        // openmetrics_parser doesn't handle colons in metric names; replace with underscores
        let metrics_text = metrics_text.replace(":", "_");

        match openmetrics_parser::prometheus::parse_prometheus(&metrics_text) {
            Ok(exposition) => {
                let exposition = transform_metrics(exposition, &metric_pack.labels);
                let exposition = add_histogram_stat_families(exposition);
                append_exposition_text(exposition, &mut families);
            }
            Err(err) => {
                warn!(
                    labels = ?metric_pack.labels,
                    err = ?err,
                    "aggregate_metrics failed to parse full worker metrics payload; retrying per family"
                );
                append_lossy_exposition_text(&metrics_text, &metric_pack.labels, &mut families);
            }
        }
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

fn append_lossy_exposition_text(
    metrics_text: &str,
    extra_labels: &[(String, String)],
    aggregated: &mut BTreeMap<String, AggregatedFamilyText>,
) {
    for block in split_metric_family_blocks(metrics_text) {
        let block_text = format!("{}\n", block.lines.join("\n"));
        match openmetrics_parser::prometheus::parse_prometheus(&block_text) {
            Ok(exposition) => {
                let exposition = transform_metrics(exposition, extra_labels);
                let exposition = add_histogram_stat_families(exposition);
                append_exposition_text(exposition, aggregated);
            }
            Err(err) => {
                warn!(
                    labels = ?extra_labels,
                    family = block.name.as_deref().unwrap_or("<unknown>"),
                    err = ?err,
                    "aggregate_metrics skipped invalid metrics family"
                );
            }
        }
    }
}

fn split_metric_family_blocks(metrics_text: &str) -> Vec<MetricFamilyBlock> {
    let mut blocks = Vec::new();
    let mut current = MetricFamilyBlock::default();

    for raw_line in metrics_text.lines() {
        let line = raw_line.trim_end();
        if line.is_empty() {
            continue;
        }

        if let Some(name) = metric_name_from_metadata_line(line) {
            if !current.lines.is_empty() && current.name.as_deref() != Some(name) {
                blocks.push(std::mem::take(&mut current));
            }
            current.name.get_or_insert_with(|| name.to_string());
            current.lines.push(line.to_string());
            continue;
        }

        if current.name.is_none() {
            current.name = sample_metric_name(line);
        }
        current.lines.push(line.to_string());
    }

    if !current.lines.is_empty() {
        blocks.push(current);
    }

    blocks
}

fn metric_name_from_metadata_line(line: &str) -> Option<&str> {
    let mut parts = line.split_whitespace();
    if parts.next()? != "#" {
        return None;
    }
    match parts.next()? {
        "HELP" | "TYPE" => parts.next(),
        _ => None,
    }
}

fn sample_metric_name(line: &str) -> Option<String> {
    let brace_idx = line.find('{').unwrap_or(line.len());
    let space_idx = line.find(' ').unwrap_or(line.len());
    let end = brace_idx.min(space_idx);
    (end > 0).then(|| line[..end].to_string())
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

fn add_histogram_stat_families(mut exposition: PrometheusExposition) -> PrometheusExposition {
    let derived_families = exposition
        .families
        .values()
        .flat_map(derive_histogram_stat_families)
        .collect::<Vec<_>>();

    for family in derived_families {
        match exposition.families.entry(family.family_name.clone()) {
            Entry::Vacant(entry) => {
                entry.insert(family);
            }
            Entry::Occupied(_) => {
                warn!(
                    family = %family.family_name,
                    "aggregate_metrics skipped derived histogram stats because the metric family already exists"
                );
            }
        }
    }

    exposition
}

fn derive_histogram_stat_families(
    family: &MetricFamily<PrometheusType, PrometheusValue>,
) -> Vec<MetricFamily<PrometheusType, PrometheusValue>> {
    if family.family_type != PrometheusType::Histogram {
        return Vec::new();
    }

    let label_names = family.get_label_names().to_vec();
    let mut quantile_samples = HISTOGRAM_QUANTILES.map(|_| Vec::new());
    let mut avg_samples = Vec::new();

    for sample in family.iter_samples() {
        let PrometheusValue::Histogram(histogram) = &sample.value else {
            continue;
        };

        let Ok(labelset) = sample.get_labelset() else {
            warn!(
                family = %family.family_name,
                "aggregate_metrics skipped histogram stats for a sample with invalid labels"
            );
            continue;
        };

        let label_values = labelset.iter_values().cloned().collect::<Vec<_>>();
        let timestamp = sample.timestamp;

        for (idx, (_, quantile)) in HISTOGRAM_QUANTILES.iter().enumerate() {
            if let Some(value) = histogram_quantile(histogram, *quantile) {
                quantile_samples[idx].push(Sample::new(
                    label_values.clone(),
                    timestamp,
                    PrometheusValue::Gauge(MetricNumber::Float(value)),
                ));
            }
        }

        if let Some(value) = histogram_average(histogram) {
            avg_samples.push(Sample::new(
                label_values,
                timestamp,
                PrometheusValue::Gauge(MetricNumber::Float(value)),
            ));
        }
    }

    let mut derived_families = Vec::new();

    for (idx, (suffix, _)) in HISTOGRAM_QUANTILES.iter().enumerate() {
        if let Some(family) = build_derived_gauge_family(
            family,
            &label_names,
            suffix,
            format!(
                "Approximate {suffix} derived from histogram {} at scrape time.",
                family.family_name
            ),
            std::mem::take(&mut quantile_samples[idx]),
        ) {
            derived_families.push(family);
        }
    }

    if let Some(family) = build_derived_gauge_family(
        family,
        &label_names,
        "avg",
        format!(
            "Average derived from histogram {} at scrape time.",
            family.family_name
        ),
        avg_samples,
    ) {
        derived_families.push(family);
    }

    derived_families
}

fn build_derived_gauge_family(
    source_family: &MetricFamily<PrometheusType, PrometheusValue>,
    label_names: &[String],
    suffix: &str,
    help: String,
    samples: Vec<Sample<PrometheusValue>>,
) -> Option<MetricFamily<PrometheusType, PrometheusValue>> {
    if samples.is_empty() {
        return None;
    }

    let family_name = format!("{}_{}", source_family.family_name, suffix);
    MetricFamily::new(
        family_name.clone(),
        label_names.to_vec(),
        PrometheusType::Gauge,
        help,
        String::new(),
    )
    .with_samples(samples)
    .map_err(|err| {
        warn!(
            family = %family_name,
            err = ?err,
            "aggregate_metrics failed to build derived histogram stats family"
        );
        err
    })
    .ok()
}

fn histogram_average(histogram: &openmetrics_parser::HistogramValue) -> Option<f64> {
    let sum = histogram.sum?.as_f64();
    let count = histogram_total_count(histogram)?;

    (count > 0.0).then_some(sum / count)
}

fn histogram_quantile(
    histogram: &openmetrics_parser::HistogramValue,
    quantile: f64,
) -> Option<f64> {
    if !(0.0..=1.0).contains(&quantile) || histogram.buckets.is_empty() {
        return None;
    }

    let total_count = histogram_total_count(histogram)?;
    if total_count <= 0.0 {
        return None;
    }

    let mut buckets = histogram.buckets.iter().collect::<Vec<_>>();
    buckets.sort_by(|left, right| left.upper_bound.total_cmp(&right.upper_bound));

    let rank = quantile * total_count;
    let mut previous_count = 0.0;
    let mut previous_upper_bound = 0.0;

    for (bucket_idx, bucket) in buckets.iter().enumerate() {
        let bucket_count = bucket.count.as_f64();
        if !bucket_count.is_finite() || bucket_count < previous_count {
            return None;
        }

        if bucket_count >= rank {
            if bucket.upper_bound.is_infinite() && bucket.upper_bound.is_sign_positive() {
                return Some(previous_upper_bound);
            }

            let lower_bound = if bucket_idx == 0 && bucket.upper_bound > 0.0 {
                0.0
            } else {
                previous_upper_bound
            };
            let observations_in_bucket = bucket_count - previous_count;
            if observations_in_bucket <= 0.0 {
                return Some(bucket.upper_bound);
            }

            let bucket_fraction =
                ((rank - previous_count) / observations_in_bucket).clamp(0.0, 1.0);
            return Some(lower_bound + (bucket.upper_bound - lower_bound) * bucket_fraction);
        }

        if bucket.upper_bound.is_finite() {
            previous_upper_bound = bucket.upper_bound;
        }
        previous_count = bucket_count;
    }

    Some(previous_upper_bound)
}

fn histogram_total_count(histogram: &openmetrics_parser::HistogramValue) -> Option<f64> {
    histogram
        .count
        .map(|count| count as f64)
        .or_else(|| histogram.buckets.last().map(|bucket| bucket.count.as_f64()))
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
