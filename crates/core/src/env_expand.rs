//! `${VAR}` placeholder expansion for provider config strings.
//!
//! Some providers embed a per-account identifier in their base URL that can't be committed to
//! `brand.json` verbatim — Infomaniak's chat endpoint is
//! `https://api.infomaniak.com/2/ai/{product_id}/openai/v1/chat/completions`, where `product_id`
//! is specific to the caller's Infomaniak account. Such a `base_url` is stored with a
//! `${INFOMANIAK_PRODUCT_ID}` placeholder and expanded from the environment at request time
//! (never baked into the DB), so the same committed catalog works for every deployment as long
//! as the env var is set (e.g. in the Docker container).

/// Replace every `${VAR}` occurrence in `s` with the value of the environment variable `VAR`.
/// An unset variable leaves the placeholder untouched, so a misconfiguration surfaces as a
/// visibly broken URL rather than a silently truncated one.
pub fn expand_env_placeholders(s: &str) -> String {
    if !s.contains("${") {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find('}') {
            Some(end) => {
                let name = &after[..end];
                match std::env::var(name) {
                    Ok(val) => out.push_str(&val),
                    Err(_) => {
                        out.push_str("${");
                        out.push_str(name);
                        out.push('}');
                    }
                }
                rest = &after[end + 1..];
            }
            None => {
                // No closing brace — emit the rest verbatim and stop.
                out.push_str(&rest[start..]);
                return out;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Same as [`expand_env_placeholders`] but operates on an `Option<String>`, the shape most
/// provider config fields have.
pub fn expand_env_placeholders_opt(s: Option<String>) -> Option<String> {
    s.map(|v| expand_env_placeholders(&v))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_placeholder_is_passthrough() {
        assert_eq!(
            expand_env_placeholders("https://api.example.com/v1"),
            "https://api.example.com/v1"
        );
    }

    #[test]
    fn expands_set_var() {
        std::env::set_var("PROVIZ_TEST_PRODUCT_ID", "acct-XXXX");
        assert_eq!(
            expand_env_placeholders(
                "https://api.infomaniak.com/2/ai/${PROVIZ_TEST_PRODUCT_ID}/openai/v1"
            ),
            "https://api.infomaniak.com/2/ai/acct-XXXX/openai/v1"
        );
        std::env::remove_var("PROVIZ_TEST_PRODUCT_ID");
    }

    #[test]
    fn unset_var_left_intact() {
        std::env::remove_var("PROVIZ_TEST_MISSING_VAR");
        assert_eq!(
            expand_env_placeholders("a/${PROVIZ_TEST_MISSING_VAR}/b"),
            "a/${PROVIZ_TEST_MISSING_VAR}/b"
        );
    }

    #[test]
    fn multiple_placeholders() {
        std::env::set_var("PROVIZ_TEST_A", "1");
        std::env::set_var("PROVIZ_TEST_B", "2");
        assert_eq!(
            expand_env_placeholders("${PROVIZ_TEST_A}-${PROVIZ_TEST_B}"),
            "1-2"
        );
        std::env::remove_var("PROVIZ_TEST_A");
        std::env::remove_var("PROVIZ_TEST_B");
    }
}
