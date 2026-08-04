/// Renders `docs/<scraped-directory>/api-graph.json` — a machine-readable dump of the full API surface.
/// Useful for tooling beyond Copilot (custom LSP overlays, web dashboards, etc.).
use anyhow::Result;
use crate::model::ApiItem;

pub fn render(items: &[ApiItem]) -> Result<String> {
    Ok(serde_json::to_string_pretty(items)?)
}
