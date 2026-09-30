//! VBScript lexer, parser, and evaluator for RASP.

pub mod placeholder {
    /// Placeholder for the VBScript language engine. The lexer arrives in
    /// Milestone 1; see docs/asp-classic-interpreter-plan.md §7.3.
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
