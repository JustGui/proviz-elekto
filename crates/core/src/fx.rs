//! Foreign-exchange rates for normalising per-brand prices to USD.
//!
//! `providers/<brand>/brand.json` may declare `"price_currency"` (default `"USD"`); the
//! model prices in that provider's `models.json` are then in that currency. The selector's
//! cost scoring and every reported cost figure (`estimated_input_cost_usd`, `/report`
//! `actual_cost_usd`, `/complete` `cost_usd`) need a single currency to be comparable, so
//! non-USD prices are converted through [`FxRates::to_usd`].
//!
//! Rates come from the European Central Bank via <https://frankfurter.dev> (free, no auth,
//! updated ~once per working day). Design:
//!
//! - A **builtin seed** table ([`FxRates::with_builtin_seed`]) covers every ECB currency, so
//!   conversion always works even with no network and an empty DB — just at a stale rate.
//! - Refresh is **lazy and single-flight**: [`FxRates::begin_refresh_if_stale`] hands out a
//!   permit at most once per [`REFRESH_TTL`] (and no more than once per [`RETRY_COOLDOWN`]
//!   after a failure). The caller runs [`FxRates::refresh_blocking`] on a background thread;
//!   the current request proceeds immediately on the existing snapshot.
//! - Every successful fetch is **persisted** (`pz_fx_rates` table) so the last-good values
//!   survive restarts and are shared between the server and the `proviz` CLI.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::RwLock;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::Deserialize;

/// Frankfurter endpoint returning `{amount, base, date, rates}` with `base=USD`, i.e. `rates`
/// maps each currency to how many of its units equal one US dollar.
pub const DEFAULT_FX_URL: &str = "https://api.frankfurter.dev/v1/latest?base=USD";

/// Rates older than this trigger a lazy refresh on the next selection.
pub const REFRESH_TTL: Duration = Duration::from_secs(3600);

/// After a failed refresh, don't try again for at least this long (avoids spawning a fetch
/// thread on every selection during a Frankfurter outage).
pub const RETRY_COOLDOWN: Duration = Duration::from_secs(300);

/// One currency's conversion factor. `per_usd` is "units of this currency per 1 USD".
#[derive(Debug, Clone)]
pub struct FxRate {
    pub currency: String,
    pub per_usd: f64,
    /// ECB rate date (`YYYY-MM-DD`) the value came from, or `"builtin"` for the seed.
    pub rate_date: String,
    pub fetched_at: DateTime<Utc>,
}

#[derive(Debug)]
struct FxSnapshot {
    /// currency (upper-case) -> units per 1 USD
    per_usd: HashMap<String, f64>,
    /// ECB rate date, or `"builtin"`.
    date: String,
    /// When these values were fetched; `None` while still on the builtin seed.
    fetched_at: Option<DateTime<Utc>>,
    /// When a refresh was last attempted (success or failure) — gates [`RETRY_COOLDOWN`].
    last_attempt: Option<DateTime<Utc>>,
}

/// Thread-safe holder of the current FX snapshot plus a single-flight refresh guard.
#[derive(Debug)]
pub struct FxRates {
    inner: RwLock<FxSnapshot>,
    refreshing: AtomicBool,
}

/// Builtin seed rates (units per 1 USD), rough mid-2020s values for every ECB/Frankfurter
/// currency. Only used until the first successful fetch; being off by a few percent here
/// just means slightly-wrong cost *scoring* for a few seconds after a cold start.
const BUILTIN_SEED: &[(&str, f64)] = &[
    ("EUR", 0.92),
    ("CHF", 0.88),
    ("GBP", 0.79),
    ("JPY", 157.0),
    ("AUD", 1.52),
    ("CAD", 1.37),
    ("NZD", 1.66),
    ("SEK", 10.6),
    ("NOK", 10.7),
    ("DKK", 6.9),
    ("PLN", 3.95),
    ("CZK", 23.2),
    ("HUF", 360.0),
    ("RON", 4.6),
    ("BGN", 1.81),
    ("ISK", 138.0),
    ("HKD", 7.8),
    ("SGD", 1.34),
    ("CNY", 7.15),
    ("INR", 84.0),
    ("KRW", 1360.0),
    ("IDR", 16200.0),
    ("MYR", 4.5),
    ("PHP", 57.0),
    ("THB", 35.0),
    ("ZAR", 18.2),
    ("BRL", 5.6),
    ("MXN", 18.5),
    ("TRY", 34.0),
    ("ILS", 3.7),
];

#[derive(Deserialize)]
struct FrankfurterResponse {
    #[allow(dead_code)]
    base: Option<String>,
    date: String,
    rates: HashMap<String, f64>,
}

impl FxRates {
    /// A snapshot pre-loaded with [`BUILTIN_SEED`] and marked stale (no `fetched_at`).
    pub fn with_builtin_seed() -> Self {
        let per_usd = BUILTIN_SEED
            .iter()
            .map(|(k, v)| ((*k).to_string(), *v))
            .collect();
        FxRates {
            inner: RwLock::new(FxSnapshot {
                per_usd,
                date: "builtin".to_string(),
                fetched_at: None,
                last_attempt: None,
            }),
            refreshing: AtomicBool::new(false),
        }
    }

    /// Overlay persisted rows (from `pz_fx_rates`) on top of the seed at startup. The newest
    /// row's `fetched_at`/`rate_date` become the snapshot's, so [`is_stale`](Self::is_stale)
    /// reflects real fetch age rather than process start.
    pub fn overlay_rows(&self, rows: &[FxRate]) {
        if rows.is_empty() {
            return;
        }
        let mut g = self.inner.write().unwrap();
        let mut newest: Option<&FxRate> = None;
        for r in rows {
            g.per_usd.insert(r.currency.to_uppercase(), r.per_usd);
            if newest.map(|n| r.fetched_at > n.fetched_at).unwrap_or(true) {
                newest = Some(r);
            }
        }
        if let Some(n) = newest {
            g.date = n.rate_date.clone();
            g.fetched_at = Some(n.fetched_at);
        }
    }

    /// Convert `amount` (denominated in `currency`) to USD. `""` and `"USD"` pass through
    /// unchanged; an unknown currency passes through with a warning.
    pub fn to_usd(&self, amount: f64, currency: &str) -> f64 {
        let c = currency.trim().to_uppercase();
        if c.is_empty() || c == "USD" {
            return amount;
        }
        let g = self.inner.read().unwrap();
        match g.per_usd.get(&c) {
            Some(rate) if *rate > 0.0 => amount / rate,
            _ => {
                tracing::warn!(currency = %c, "no FX rate available; treating price as USD");
                amount
            }
        }
    }

    /// True when the snapshot has never been fetched or is older than [`REFRESH_TTL`].
    pub fn is_stale(&self) -> bool {
        let g = self.inner.read().unwrap();
        match g.fetched_at {
            None => true,
            Some(t) => {
                Utc::now()
                    .signed_duration_since(t)
                    .to_std()
                    .unwrap_or(REFRESH_TTL)
                    >= REFRESH_TTL
            }
        }
    }

    /// Try to claim the single-flight refresh permit. Returns `true` only when the snapshot is
    /// stale, the retry cooldown has elapsed, and no other refresh is in progress. The caller
    /// MUST call [`finish_refresh`](Self::finish_refresh) when done.
    pub fn begin_refresh_if_stale(&self) -> bool {
        {
            let g = self.inner.read().unwrap();
            let stale = match g.fetched_at {
                None => true,
                Some(t) => {
                    Utc::now()
                        .signed_duration_since(t)
                        .to_std()
                        .unwrap_or(REFRESH_TTL)
                        >= REFRESH_TTL
                }
            };
            if !stale {
                return false;
            }
            if let Some(a) = g.last_attempt {
                if Utc::now()
                    .signed_duration_since(a)
                    .to_std()
                    .unwrap_or(RETRY_COOLDOWN)
                    < RETRY_COOLDOWN
                {
                    return false;
                }
            }
        }
        self.refreshing
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// Release the single-flight permit taken by [`begin_refresh_if_stale`](Self::begin_refresh_if_stale).
    pub fn finish_refresh(&self) {
        self.refreshing.store(false, Ordering::Release);
    }

    /// Blocking fetch from `url` (Frankfurter `base=USD` shape). On success swaps the in-memory
    /// snapshot and returns the rows for the caller to persist; on failure the snapshot is
    /// untouched (last-good values stay) and only `last_attempt` is bumped.
    pub fn refresh_blocking(&self, url: &str) -> Result<Vec<FxRate>, String> {
        self.inner.write().unwrap().last_attempt = Some(Utc::now());

        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(|e| format!("failed to build HTTP client: {e}"))?;
        let resp = client
            .get(url)
            .send()
            .map_err(|e| format!("fx fetch failed: {e}"))?
            .error_for_status()
            .map_err(|e| format!("fx fetch returned error status: {e}"))?;
        let parsed: FrankfurterResponse = resp
            .json()
            .map_err(|e| format!("failed to parse fx response: {e}"))?;

        if parsed.rates.is_empty() {
            return Err("fx response had no rates".to_string());
        }

        let now = Utc::now();
        let rows: Vec<FxRate> = parsed
            .rates
            .iter()
            .filter(|(_, v)| **v > 0.0)
            .map(|(k, v)| FxRate {
                currency: k.to_uppercase(),
                per_usd: *v,
                rate_date: parsed.date.clone(),
                fetched_at: now,
            })
            .collect();

        {
            let mut g = self.inner.write().unwrap();
            for r in &rows {
                g.per_usd.insert(r.currency.clone(), r.per_usd);
            }
            g.date = parsed.date.clone();
            g.fetched_at = Some(now);
        }

        tracing::info!(
            date = %parsed.date,
            count = rows.len(),
            "fx rates refreshed"
        );
        Ok(rows)
    }

    /// `{base, date, fetched_at, stale, rates}` for `GET /fx/rates`.
    pub fn snapshot_json(&self) -> serde_json::Value {
        let g = self.inner.read().unwrap();
        let stale = match g.fetched_at {
            None => true,
            Some(t) => {
                Utc::now()
                    .signed_duration_since(t)
                    .to_std()
                    .unwrap_or(REFRESH_TTL)
                    >= REFRESH_TTL
            }
        };
        serde_json::json!({
            "base": "USD",
            "date": g.date,
            "fetched_at": g.fetched_at,
            "stale": stale,
            "rates": g.per_usd,
        })
    }
}

impl Default for FxRates {
    fn default() -> Self {
        Self::with_builtin_seed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usd_passes_through() {
        let fx = FxRates::with_builtin_seed();
        assert_eq!(fx.to_usd(12.0, "USD"), 12.0);
        assert_eq!(fx.to_usd(12.0, ""), 12.0);
        assert_eq!(fx.to_usd(12.0, "usd"), 12.0);
    }

    #[test]
    fn builtin_seed_converts_eur() {
        let fx = FxRates::with_builtin_seed();
        // 0.92 EUR per USD → 0.92 EUR == 1 USD
        let usd = fx.to_usd(0.92, "EUR");
        assert!((usd - 1.0).abs() < 1e-9, "got {usd}");
    }

    #[test]
    fn unknown_currency_passes_through() {
        let fx = FxRates::with_builtin_seed();
        assert_eq!(fx.to_usd(5.0, "XYZ"), 5.0);
    }

    #[test]
    fn overlay_rows_updates_rate_and_freshness() {
        let fx = FxRates::with_builtin_seed();
        assert!(fx.is_stale());
        fx.overlay_rows(&[FxRate {
            currency: "EUR".into(),
            per_usd: 0.80,
            rate_date: "2026-08-27".into(),
            fetched_at: Utc::now(),
        }]);
        assert!(!fx.is_stale());
        let usd = fx.to_usd(0.80, "eur");
        assert!((usd - 1.0).abs() < 1e-9, "got {usd}");
    }

    #[test]
    fn parses_frankfurter_body() {
        let body =
            r#"{"amount":1.0,"base":"USD","date":"2026-08-27","rates":{"EUR":0.857,"CHF":0.805}}"#;
        let parsed: FrankfurterResponse = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.date, "2026-08-27");
        assert_eq!(parsed.rates.len(), 2);
        assert!((parsed.rates["EUR"] - 0.857).abs() < 1e-9);
    }

    #[test]
    fn single_flight_permit() {
        let fx = FxRates::with_builtin_seed();
        assert!(fx.begin_refresh_if_stale());
        // Second caller is blocked while the first holds the permit.
        assert!(!fx.begin_refresh_if_stale());
        fx.finish_refresh();
    }

    #[test]
    fn snapshot_json_shape() {
        let fx = FxRates::with_builtin_seed();
        let j = fx.snapshot_json();
        assert_eq!(j["base"], "USD");
        assert_eq!(j["date"], "builtin");
        assert_eq!(j["stale"], true);
        assert!(j["rates"]["EUR"].is_number());
    }
}
