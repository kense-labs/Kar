use super::FailureClass as Class;
use regex::Regex;
use std::sync::LazyLock;

pub(crate) fn context_overflow(reason: &str) -> bool {
    static OVERFLOW: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
        r"(?i)^(?:Internal error: )?(?:This model's maximum context length is [0-9,]+ tokens\. However, (?:your messages resulted in|you requested) [0-9,]+ tokens\b|Your input exceeds the context window of this model(?:\.|$)|Prompt (?:is )?too long(?:$|[.:]|\s+·))"
    ).unwrap()
    });
    OVERFLOW.is_match(reason)
}

pub(crate) fn prose(reason: &str) -> Class {
    if context_overflow(reason) {
        return Class::InputTooLarge;
    }
    // Ordered like upstream's ACP fallback. Prose supplies a class only, never
    // an HTTP status, credential diagnosis or reset timestamp.
    static RULES: LazyLock<Vec<(Regex, Class)>> = LazyLock::new(|| {
        [
        (r"\b401\b|authenticat(?:e|ed|ion)|\boauth\b|unauthorized|invalid (?:user )?(?:x-)?api[- ]?key|x-api-key|log(?:ged)? ?in", Class::Auth),
        (r"credit balance is too low", Class::Billing),
        (r"reached your specified api usage limits", Class::Quota),
        (r"\b429\b|rate.?limit", Class::RateLimited),
        (r"quota|usage limit", Class::Quota),
        (r"billing", Class::Billing),
        (r"\b400\b|invalid_request|issue with the selected model|model .* (?:does not|doesn't|may not) exist|unknown model|invalid model", Class::InvalidRequest),
        (r"\b529\b|\b503\b|overloaded", Class::Overloaded),
        (r"\b\d{3}\b|error", Class::Provider),
    ].into_iter().map(|(pattern, class)| (Regex::new(&format!("(?i){pattern}")).unwrap(), class)).collect()
    });
    RULES
        .iter()
        .find(|(r, _)| r.is_match(reason))
        .map_or(Class::Unknown, |(_, c)| *c)
}
