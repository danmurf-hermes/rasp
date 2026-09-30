//! HTTP server and request-to-response integration for RASP.

pub mod placeholder {
    /// Placeholder for HTTP integration. The server wires up in
    /// Milestone 1; see docs/asp-classic-interpreter-plan.md §7.11.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Placeholder;

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn placeholder_compares_by_value() {
            assert_eq!(Placeholder, Placeholder);
        }
    }
}
