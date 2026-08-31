//! OpenAPI 3.1 contract helpers: deterministic rendering, ETag, and drift.

/// Regenerate the canonical OpenAPI document with the given server URL.
/// The document itself must be defined where the handler paths live.
pub fn set_servers(
    doc: &mut utoipa::openapi::OpenApi,
    api_server_url: &str,
) {
    doc.info.version = crate::api::OPENAPI_CONTRACT_VERSION.to_string();
    doc.servers = Some(vec![
        utoipa::openapi::server::ServerBuilder::new()
            .url(api_server_url)
            .description(Some(
                "Deployed monitor location; the API is unauthenticated and may be \
                 restricted at the reverse proxy."
                    .to_string(),
            ))
            .build(),
    ]);
}

/// Serialize the canonical document deterministically (sorted keys).
pub fn render_document(doc: &utoipa::openapi::OpenApi) -> String {
    let value = serde_json::to_value(doc).expect("openapi serialization");
    let sorted = sort_json(value);
    serde_json::to_string(&sorted).expect("openapi json rendering")
}

pub fn sort_json(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut sorted: std::collections::BTreeMap<String, serde_json::Value> =
                map.into_iter().collect();
            for (_, v) in sorted.iter_mut() {
                *v = sort_json(v.clone());
            }
            serde_json::Value::Object(sorted.into_iter().collect())
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.into_iter().map(sort_json).collect())
        }
        other => other,
    }
}

/// Compute a change-detecting ETag for the rendered contract.
pub fn contract_etag(body: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    body.hash(&mut hasher);
    format!("\"{:016x}\"", hasher.finish())
}
