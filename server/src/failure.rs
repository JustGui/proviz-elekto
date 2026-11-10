//! Why a provider call failed, in a small fixed vocabulary.
//!
//! Provider error bodies can echo customer input, so a body is read only to classify it here
//! and is then dropped. What leaves the gateway (the `/complete` error `detail`, logs, metrics)
//! is the HTTP status, the [`FailureReason`] and at most a short provider error code that
//! matches [`provider_code`]'s strict token shape.

/// The reason vocabulary shared by `/complete` error details and the
/// `proviz_provider_failures_total{reason}` metric. Keep it small and stable: alert rules
/// match on these strings.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FailureReason {
    /// The account or key has no balance left (HTTP 402, or a billing body).
    CreditsExhausted,
    /// A daily/monthly allowance or a provider "insufficient quota" signal.
    Quota,
    /// A short-window rate limit (RPM/TPM, 429).
    RateLimited,
    /// Bad, missing or revoked credentials (401/403, or an unset key env var).
    Auth,
    /// The provider rejected the request itself (400/404/413/422).
    BadRequest,
    /// Upstream 5xx or any other unexpected status.
    Server,
    /// The provider did not answer in time.
    Timeout,
    /// Connection-level failure before an HTTP status.
    Network,
    /// A 2xx whose body could not be used.
    InvalidResponse,
    /// The catalog has no usable endpoint for the brand.
    Config,
}

impl FailureReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CreditsExhausted => "credits_exhausted",
            Self::Quota => "quota",
            Self::RateLimited => "rate_limited",
            Self::Auth => "auth",
            Self::BadRequest => "bad_request",
            Self::Server => "server",
            Self::Timeout => "timeout",
            Self::Network => "network",
            Self::InvalidResponse => "invalid_response",
            Self::Config => "config",
        }
    }
}

impl std::fmt::Display for FailureReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Billing wording seen in provider bodies when the balance is gone. Matched case-insensitively.
const CREDIT_PATTERNS: &[&str] = &[
    "insufficient balance",
    "insufficient_balance",
    "insufficient credit",
    "insufficient_credit",
    "insufficient funds",
    "insufficient_funds",
    "not enough credit",
    "credit balance",
    "credits exhausted",
    "out of credits",
    "no credits",
    "payment required",
    "payment_required",
    "account balance",
    "balance is too low",
    "billing hard limit",
    "exceeded your current quota",
    "top up",
];

/// Allowance wording: a daily/monthly cap or a generic quota signal, not a balance.
const QUOTA_PATTERNS: &[&str] = &[
    "insufficient quota",
    "insufficient_quota",
    "quota exceeded",
    "quota_exceeded",
    "exceeded quota",
    "daily limit",
    "monthly limit",
    "requests per day",
    "tokens per day",
];

/// Cloudflare API error codes that mean the account allowance is used up. 4006: "you have used
/// up your daily free allocation of 10,000 neurons".
const CLOUDFLARE_CREDIT_CODES: &[&str] = &["4006"];

fn brand_prefix(brand: &str) -> &str {
    brand.split('-').next().unwrap_or(brand)
}

/// Classify a non-2xx provider response. `body` is used only for matching here and must never
/// be stored or logged by the caller.
pub fn classify_http(brand: &str, status: u16, body: &str) -> FailureReason {
    let lower = body.to_ascii_lowercase();
    let has = |patterns: &[&str]| patterns.iter().any(|p| lower.contains(p));
    let code = provider_code(body);

    if status == 402
        || (matches!(status, 400 | 403 | 429) && has(CREDIT_PATTERNS))
        || (brand_prefix(brand) == "cloudflare"
            && code
                .as_deref()
                .is_some_and(|c| CLOUDFLARE_CREDIT_CODES.contains(&c)))
    {
        return FailureReason::CreditsExhausted;
    }
    if matches!(status, 400 | 403 | 413 | 429) && has(QUOTA_PATTERNS) {
        return FailureReason::Quota;
    }
    if status == 429
        || (brand_prefix(brand) == "typesafe" && status == 529)
        || body.contains("rate_limit_exceeded")
        || body.contains("tokens per minute")
    {
        return FailureReason::RateLimited;
    }
    match status {
        401 | 403 => FailureReason::Auth,
        408 | 504 => FailureReason::Timeout,
        400 | 404 | 405 | 409 | 413 | 422 => FailureReason::BadRequest,
        _ => FailureReason::Server,
    }
}

/// A short provider error code from a JSON error body, safe to surface: OpenAI-style
/// `error.code` / `error.type`, Cloudflare-style `errors[0].code`, or a top-level `code`.
/// Only plain tokens (`[A-Za-z0-9_.-]`, at most 48 chars) or integers qualify, so free text
/// that might quote the request never passes through.
pub fn provider_code(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let candidates = [
        &v["error"]["code"],
        &v["error"]["type"],
        &v["errors"][0]["code"],
        &v["code"],
    ];
    let code = candidates.into_iter().find_map(|c| match c {
        serde_json::Value::Number(n) => n.as_i64().map(|n| n.to_string()),
        serde_json::Value::String(s)
            if !s.is_empty()
                && s.len() <= 48
                && s.chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.' | '-')) =>
        {
            Some(s.clone())
        }
        _ => None,
    });
    code
}

/// The safe one-line description used for `/complete` error details and logs.
pub fn describe_http(
    brand: &str,
    status: u16,
    reason: FailureReason,
    code: Option<&str>,
) -> String {
    match code {
        Some(code) => format!("{brand} HTTP {status} ({reason}, code {code})"),
        None => format!("{brand} HTTP {status} ({reason})"),
    }
}

#[cfg(test)]
mod tests {
    use super::FailureReason as R;
    use super::*;

    #[test]
    fn classifies_status_and_body() {
        let cases: &[(&str, u16, &str, R)] = &[
            ("openrouter", 402, "{}", R::CreditsExhausted),
            ("deepinfra", 402, r#"{"detail":"..."}"#, R::CreditsExhausted),
            (
                "requesty",
                400,
                r#"{"error":{"message":"Insufficient balance"}}"#,
                R::CreditsExhausted,
            ),
            (
                "openai",
                429,
                r#"{"error":{"message":"You exceeded your current quota, please check your plan and billing details","type":"insufficient_quota"}}"#,
                R::CreditsExhausted,
            ),
            (
                "cloudflare",
                429,
                r#"{"success":false,"errors":[{"code":4006,"message":"..."}]}"#,
                R::CreditsExhausted,
            ),
            (
                "cloudflare-backup",
                403,
                r#"{"errors":[{"code":10000,"message":"Out of credits"}]}"#,
                R::CreditsExhausted,
            ),
            (
                "mockbrand",
                429,
                r#"{"error":{"message":"INSUFFICIENT QUOTA"}}"#,
                R::Quota,
            ),
            (
                "groq",
                429,
                r#"{"error":{"message":"Rate limit reached on tokens per day (TPD)"}}"#,
                R::Quota,
            ),
            (
                "groq",
                413,
                r#"{"error":{"code":"rate_limit_exceeded","message":"tokens per minute"}}"#,
                R::RateLimited,
            ),
            ("mistral", 429, "rate limited", R::RateLimited),
            ("typesafe", 529, "", R::RateLimited),
            ("typesafe-alt", 529, "", R::RateLimited),
            ("scaleway", 529, "", R::Server),
            ("ovh", 401, "", R::Auth),
            ("ovh", 403, r#"{"error":{"message":"forbidden"}}"#, R::Auth),
            ("ovh", 504, "", R::Timeout),
            ("ovh", 422, "", R::BadRequest),
            ("ovh", 500, "", R::Server),
            ("ovh", 503, "billing", R::Server),
        ];
        for (brand, status, body, want) in cases {
            assert_eq!(
                classify_http(brand, *status, body),
                *want,
                "{brand} {status} {body}"
            );
        }
    }

    #[test]
    fn provider_code_keeps_only_plain_tokens() {
        assert_eq!(
            provider_code(r#"{"error":{"code":"insufficient_quota"}}"#).as_deref(),
            Some("insufficient_quota")
        );
        assert_eq!(
            provider_code(r#"{"errors":[{"code":4006,"message":"x"}]}"#).as_deref(),
            Some("4006")
        );
        // Free text or anything that could carry request content is dropped.
        assert_eq!(
            provider_code(r#"{"error":{"code":"bad input: Le président a dit"}}"#),
            None
        );
        assert_eq!(provider_code(r#"{"error":{"code":""}}"#), None);
        assert_eq!(provider_code("not json"), None);
        assert_eq!(
            provider_code(&format!(r#"{{"code":"{}"}}"#, "a".repeat(49))),
            None
        );
    }

    #[test]
    fn description_has_no_body() {
        let body = r#"{"error":{"message":"echo: SECRET-CUSTOMER-TEXT","code":"invalid_request"}}"#;
        let reason = classify_http("openrouter", 400, body);
        let line = describe_http("openrouter", 400, reason, provider_code(body).as_deref());
        assert_eq!(
            line,
            "openrouter HTTP 400 (bad_request, code invalid_request)"
        );
        assert!(!line.contains("SECRET"));
    }
}
