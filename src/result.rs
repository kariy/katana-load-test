use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use tabled::settings::Style;

use crate::BenchmarkMetrics;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkResult {
    pub metrics: BenchmarkMetrics,
}

pub struct ResultsStorage {
    storage_path: PathBuf,
}

impl ResultsStorage {
    pub fn new(path: PathBuf) -> Self {
        fs::create_dir_all(&path).unwrap();
        Self { storage_path: path }
    }

    pub async fn save_result(&self, result: BenchmarkResult) -> Result<()> {
        let filename = "latest_benchmark.json";
        let path = self.storage_path.join(filename);

        // Save current as previous before writing new result
        if path.exists() {
            let previous_path = self.storage_path.join("previous_benchmark.json");
            fs::copy(&path, previous_path)?;
        }

        let json = serde_json::to_string_pretty(&result)?;
        fs::write(path, json)?;

        Ok(())
    }

    async fn load_previous(&self) -> Result<Option<BenchmarkResult>> {
        let path = self.storage_path.join("previous_benchmark.json");
        if !path.exists() {
            return Ok(None);
        }

        let content = fs::read_to_string(path)?;
        let result: BenchmarkResult = serde_json::from_str(&content)?;
        Ok(Some(result))
    }

    pub async fn generate_comparison_report(&self, current: &BenchmarkResult) -> Result<String> {
        use tabled::{Table, Tabled};

        #[derive(Tabled)]
        struct Metrics {
            metric: String,
            current: String,
            previous: String,
            change: String,
        }

        let previous = self.load_previous().await?;

        let metrics = vec![
            Metrics {
                metric: "TPS".to_string(),
                current: format!("{:.2}", current.metrics.tps),
                previous: match &previous {
                    Some(prev) => format!("{:.2}", prev.metrics.tps),
                    None => "-".to_string(),
                },
                change: match &previous {
                    Some(prev) => {
                        let change =
                            (current.metrics.tps - prev.metrics.tps) / prev.metrics.tps * 100.0;
                        format!(
                            "{:.2}% {}",
                            change.abs(),
                            if change >= 0.0 {
                                "increase"
                            } else {
                                "decrease"
                            }
                        )
                    }
                    None => "-".to_string(),
                },
            },
            Metrics {
                metric: "Latency".to_string(),
                current: format!("{:?}", current.metrics.avg_latency),
                previous: match &previous {
                    Some(prev) => format!("{:?}", prev.metrics.avg_latency),
                    None => "-".to_string(),
                },
                change: match &previous {
                    Some(prev) => {
                        let change = (current.metrics.avg_latency.as_secs_f64()
                            - prev.metrics.avg_latency.as_secs_f64())
                            / prev.metrics.avg_latency.as_secs_f64()
                            * 100.0;
                        format!(
                            "{:.2}% {}",
                            change.abs(),
                            if change >= 0.0 {
                                "increase"
                            } else {
                                "decrease"
                            }
                        )
                    }
                    None => "-".to_string(),
                },
            },
            Metrics {
                metric: "Total Duration".to_string(),
                current: format!("{:?}", current.metrics.total_duration),
                previous: match &previous {
                    Some(prev) => format!("{:?}", prev.metrics.total_duration),
                    None => "-".to_string(),
                },
                change: "-".to_string(),
            },
        ];

        let mut table = Table::new(metrics);
        table.with(Style::psql());

        let mut report = String::from("\nBenchmark Report:\n");
        report.push_str("========================\n\n");
        report.push_str(&table.to_string());

        if previous.is_none() {
            report.push_str("\nNo previous benchmark data available for comparison.\n");
        }

        Ok(report)
    }
}
