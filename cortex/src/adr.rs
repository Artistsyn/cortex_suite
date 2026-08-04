use crate::model::Adr;

/// Format an ADR as a dense context string for injection into prompts.
pub fn format_for_context(adr: &Adr) -> String {
    format!(
        "## ADR-{:03}: {} [{}]\nContext: {}\nDecision: {}\n",
        adr.adr_number, adr.title, adr.status, adr.context, adr.decision
    )
}
