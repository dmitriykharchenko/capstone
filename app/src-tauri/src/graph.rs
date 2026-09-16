//! Fixed, allow-listed requests for the paper graph sidecar API.

use serde_json::Value;

use crate::error::CommandError;

pub fn neighborhood_request(
    node_id: String,
    max_depth: Option<u32>,
) -> Result<Value, CommandError> {
    let depth = max_depth.unwrap_or(1);
    if node_id.trim().is_empty() || depth > 5 {
        return Err(CommandError::Bridge(
            "graph node id must be non-empty and max depth must be at most 5".to_string(),
        ));
    }
    Ok(serde_json::json!({
        "command": "graph_query",
        "query": "neighborhood",
        "node_id": node_id,
        "max_depth": depth,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_fixed_neighborhood_request_with_deterministic_default_depth() {
        assert_eq!(
            neighborhood_request("paper-a".to_string(), None).unwrap(),
            serde_json::json!({
                "command": "graph_query",
                "query": "neighborhood",
                "node_id": "paper-a",
                "max_depth": 1,
            })
        );
    }

    #[test]
    fn rejects_blank_identifiers_and_out_of_bounds_depth() {
        assert!(neighborhood_request("  ".to_string(), None).is_err());
        assert!(neighborhood_request("paper-a".to_string(), Some(6)).is_err());
    }
}
