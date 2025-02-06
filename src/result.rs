use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

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
        let previous = self.load_previous().await?;

        let mut report = String::new();
        report.push_str("\nBenchmark Report:\n");
        report.push_str("========================\n\n");

        report.push_str(&format!("TPS: {:.2}\n", current.metrics.tps));
        report.push_str(&format!("Avg Latency: {:?}\n", current.metrics.avg_latency));
        report.push_str(&format!(
            "Total Duration: {:?}\n",
            current.metrics.total_duration
        ));

        if let Some(prev) = previous {
            // Calculate percentage changes
            let tps_change = (current.metrics.tps - prev.metrics.tps) / prev.metrics.tps * 100.0;
            let latency_change = (current.metrics.avg_latency.as_secs_f64()
                - prev.metrics.avg_latency.as_secs_f64())
                / prev.metrics.avg_latency.as_secs_f64()
                * 100.0;

            report.push_str("\nChanges:\n");
            report.push_str(&format!(
                "TPS: {:.2}% {}\n",
                tps_change.abs(),
                if tps_change >= 0.0 {
                    "increase"
                } else {
                    "decrease"
                }
            ));
            report.push_str(&format!(
                "Latency: {:.2}% {}\n",
                latency_change.abs(),
                if latency_change >= 0.0 {
                    "increase"
                } else {
                    "decrease"
                }
            ));
        } else {
            report.push_str("\nNo previous benchmark data available for comparison.\n");
        }

        Ok(report)
    }
}
