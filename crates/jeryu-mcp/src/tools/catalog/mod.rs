//! The 16-tool catalog: kinds, definitions, input schemas, and argument normalization.
//!
//! The catalog uses repository and pull-request identifiers that match the local
//! GitHub-compatible API surface.

mod definitions;
mod input_schema;
mod kind;
mod session;

pub(crate) use definitions::tool_definition;
#[allow(unused_imports)] // Preserves the historic `catalog::{ToolDefinition, ToolKind}` paths.
pub(crate) use kind::{ToolDefinition, ToolKind};

use crate::backend::ToolDescriptor;
use crate::tools::CATALOG;

/// Build every tool descriptor in `CATALOG` order.
pub(crate) fn catalog() -> Vec<ToolDescriptor> {
    CATALOG
        .iter()
        .filter_map(|id| tool_definition(id).map(|def| def.descriptor(id)))
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::tool_definition;

    /// `bot.activity` takes an optional limit: 50 unless asked, at most 200, and
    /// nothing but an integer.
    #[test]
    fn bot_activity_takes_a_bounded_limit() {
        let tool = tool_definition("bot.activity").expect("in the catalog");
        let limit = |args| tool.normalize_args(args).map(|out| out["limit"].clone());
        assert_eq!(limit(json!({})), Some(json!(50)));
        assert_eq!(limit(json!({ "limit": 5 })), Some(json!(5)));
        assert_eq!(limit(json!({ "limit": 10_000 })), Some(json!(200)));
        assert_eq!(limit(json!({ "limit": "ten" })), None);
    }
}
