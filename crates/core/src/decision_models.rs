//! Native Jev decision models; Jev Router is a generative meta-router, not Jev.
pub fn is_jev(model: &str) -> bool {
    matches!(
        model,
        "jev-latest" | "typesafe/jev-1.13" | "typesafe/jev-1.13.0" | "~typesafe/jev-latest"
    )
}

pub fn uses_systemone(brand: &str, model: &str) -> bool {
    let provider = brand.split('-').next().unwrap_or(brand);
    provider == "typesafe" || (matches!(provider, "orcarouter" | "openrouter") && is_jev(model))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn routes_only_supported_native_decision_models() {
        assert!(uses_systemone("typesafe", "jev-latest"));
        assert!(uses_systemone("orcarouter-backup", "typesafe/jev-1.13"));
        assert!(uses_systemone("openrouter", "~typesafe/jev-latest"));
        assert!(!uses_systemone("openrouter", "typesafe/jev-router"));
        assert!(!uses_systemone("requesty", "typesafe/jev-1.13.0"));
        assert!(!uses_systemone("openrouter", "some-chat-model"));
    }
}
