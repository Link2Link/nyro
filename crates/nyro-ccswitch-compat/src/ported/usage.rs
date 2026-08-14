// Minimal response usage parser retained for a source conversion regression test.
// Copyright (c) 2025 Jason Young. Licensed under MIT.

pub(crate) mod parser {
    use serde_json::Value;

    pub(crate) const SESSION_REQUEST_ID_PREFIX: &str = "session:";

    #[derive(Debug, Clone, Default)]
    pub(crate) struct TokenUsage {
        pub(crate) input_tokens: u32,
        pub(crate) output_tokens: u32,
        pub(crate) cache_read_tokens: u32,
        pub(crate) cache_creation_tokens: u32,
        pub(crate) model: Option<String>,
        pub(crate) message_id: Option<String>,
    }

    impl TokenUsage {
        pub(crate) fn from_claude_response(body: &Value) -> Option<Self> {
            let usage = body.get("usage")?;
            Some(Self {
                input_tokens: usage.get("input_tokens")?.as_u64()? as u32,
                output_tokens: usage.get("output_tokens")?.as_u64()? as u32,
                cache_read_tokens: usage
                    .get("cache_read_input_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as u32,
                cache_creation_tokens: usage
                    .get("cache_creation_input_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as u32,
                model: body.get("model").and_then(Value::as_str).map(str::to_owned),
                message_id: body.get("id").and_then(Value::as_str).map(str::to_owned),
            })
        }

        pub(crate) fn dedup_request_id(&self, scope: Option<(&str, &str)>) -> String {
            self.message_id
                .as_ref()
                .map(|message_id| match scope {
                    Some((app_type, provider_id)) => {
                        format!("{SESSION_REQUEST_ID_PREFIX}{app_type}:{provider_id}:{message_id}")
                    }
                    None => format!("{SESSION_REQUEST_ID_PREFIX}{message_id}"),
                })
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string())
        }
    }
}
