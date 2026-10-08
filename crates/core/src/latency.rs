//! Bounded persisted call samples; exponentially decayed, nonnegative size regression.
use crate::storage::CatalogStorage;
use chrono::Utc;
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

const MAX_SAMPLES: usize = 128;
const HALF_LIFE_SECS: f64 = 86_400.0;
const MAX_AGE_SECS: i64 = 7 * 86_400;
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LatencySample {
    pub at: i64,
    pub input: u64,
    pub output: u64,
    pub elapsed_ms: u32,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LatencyHistory {
    pub model_id: Uuid,
    pub key_id: Option<Uuid>,
    pub samples: Vec<LatencySample>,
}
impl LatencyHistory {
    pub fn bucket(&self) -> String {
        format!(
            "{}:{}",
            self.model_id,
            self.key_id.map(|k| k.to_string()).unwrap_or_default()
        )
    }
    pub fn predict(&self, input: u64, output: Option<u32>) -> Option<f64> {
        let now = Utc::now().timestamp();
        let samples: Vec<_> = self
            .samples
            .iter()
            .filter(|s| now - s.at <= MAX_AGE_SECS)
            .collect();
        if samples.is_empty() {
            return None;
        }
        // An omitted output budget uses the observed mean, rather than a model's context limit.
        let output = output.map(f64::from).unwrap_or_else(|| {
            samples.iter().map(|s| s.output as f64).sum::<f64>() / samples.len() as f64
        });
        let mut a = [[0.0; 3]; 3];
        let mut b = [0.0; 3];
        let sample_count = samples.len();
        for (index, s) in samples.into_iter().enumerate() {
            // Sample-order decay lets fresh benchmarks reflect recovery within a handful
            // of calls even when the old slowdown happened in the same hour. Time decay
            // also ages quiet endpoints; history remains available for diagnostics.
            let weight = 2.0_f64.powf(-((now - s.at).max(0) as f64) / HALF_LIFE_SECS)
                * 0.8_f64.powi((sample_count - index - 1) as i32);
            let x = [1.0, s.input as f64 / 1000.0, s.output as f64 / 100.0];
            for i in 0..3 {
                b[i] += weight * x[i] * s.elapsed_ms as f64;
                for j in 0..3 {
                    a[i][j] += weight * x[i] * x[j];
                }
            }
        }
        // Tiny ridge stabilises identical sizes; coordinate descent enforces nonnegative
        // overhead/prefill/decode time and never predicts negative elapsed time.
        let mut beta = [0.0; 3];
        for _ in 0..300 {
            for i in 0..3 {
                let residual = b[i]
                    - (0..3)
                        .filter(|&j| j != i)
                        .map(|j| a[i][j] * beta[j])
                        .sum::<f64>();
                beta[i] = (residual / (a[i][i] + 0.001)).max(0.0);
            }
        }
        Some((beta[0] + beta[1] * input as f64 / 1000.0 + beta[2] * output / 100.0).max(1.0))
    }
    pub fn percentile(&self, fraction: f64) -> Option<u32> {
        let now = Utc::now().timestamp();
        let mut values: Vec<_> = self
            .samples
            .iter()
            .filter(|s| now - s.at <= MAX_AGE_SECS)
            .map(|s| s.elapsed_ms)
            .collect();
        values.sort_unstable();
        if values.is_empty() {
            None
        } else {
            Some(
                values[((values.len() as f64 * fraction).ceil() as usize)
                    .saturating_sub(1)
                    .min(values.len() - 1)],
            )
        }
    }
}
pub struct LatencyTracker {
    histories: DashMap<(Uuid, Option<Uuid>), Arc<Mutex<LatencyHistory>>>,
    storage: Arc<dyn CatalogStorage>,
}
impl LatencyTracker {
    pub fn new(storage: Arc<dyn CatalogStorage>) -> Self {
        let histories = DashMap::new();
        match storage.load_latency_samples() {
            Ok(rows) => {
                for row in rows {
                    histories.insert((row.model_id, row.key_id), Arc::new(Mutex::new(row)));
                }
            }
            Err(e) => tracing::warn!(error=%e, "failed to load latency history"),
        }
        Self { histories, storage }
    }
    pub fn predict(
        &self,
        model: Uuid,
        key: Option<Uuid>,
        input: u64,
        output: Option<u32>,
    ) -> Option<f64> {
        let history = self.histories.get(&(model, key))?.value().clone();
        let prediction = history.lock().unwrap().predict(input, output);
        prediction
    }
    pub fn record(&self, model: Uuid, key: Option<Uuid>, input: u64, output: u64, elapsed_ms: u32) {
        if elapsed_ms == 0 {
            return;
        }
        let history = self
            .histories
            .entry((model, key))
            .or_insert_with(|| {
                Arc::new(Mutex::new(LatencyHistory {
                    model_id: model,
                    key_id: key,
                    samples: Vec::new(),
                }))
            })
            .value()
            .clone();
        let mut history = history.lock().unwrap();
        let at = Utc::now().timestamp();
        history.samples.retain(|s| at - s.at <= MAX_AGE_SECS);
        history.samples.push(LatencySample {
            at,
            input,
            output,
            elapsed_ms,
        });
        if history.samples.len() > MAX_SAMPLES {
            let excess = history.samples.len() - MAX_SAMPLES;
            history.samples.drain(..excess);
        }
        // Serialize writes per bucket so older snapshots cannot overwrite concurrent reports.
        if let Err(e) = self.storage.save_latency_samples(&history) {
            tracing::warn!(error=%e, "failed to save latency history");
        }
    }
    pub fn metrics(&self, model: Uuid, key: Option<Uuid>) -> (Option<u32>, Option<u32>) {
        let Some(h) = self.histories.get(&(model, key)).map(|h| h.value().clone()) else {
            return (None, None);
        };
        let h = h.lock().unwrap();
        (h.percentile(0.5), h.percentile(0.95))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn long_output_prediction_reverses_short_call_ranking() {
        let history = |overhead: u32, per_output: u32| LatencyHistory {
            model_id: Uuid::new_v4(),
            key_id: None,
            samples: [10, 100, 500, 1000]
                .into_iter()
                .flat_map(|output| {
                    [1000, 8000].map(move |input| LatencySample {
                        at: Utc::now().timestamp(),
                        input,
                        output,
                        elapsed_ms: overhead
                            + (input / 1000) as u32 * 20
                            + output as u32 * per_output,
                    })
                })
                .collect(),
        };
        let slow_decode = history(100, 30);
        let fast_decode = history(1000, 2);
        assert!(slow_decode.predict(1000, Some(10)) < fast_decode.predict(1000, Some(10)));
        assert!(slow_decode.predict(8000, Some(1000)) > fast_decode.predict(8000, Some(1000)));
        assert!((fast_decode.predict(8000, Some(1000)).unwrap() - 3160.0).abs() < 100.0);
    }
    #[test]
    fn stale_samples_do_not_predict() {
        let h = LatencyHistory {
            model_id: Uuid::new_v4(),
            key_id: None,
            samples: vec![LatencySample {
                at: Utc::now().timestamp() - MAX_AGE_SECS - 1,
                input: 1,
                output: 1,
                elapsed_ms: 20,
            }],
        };
        assert_eq!(h.predict(1, Some(1)), None);
    }
}
