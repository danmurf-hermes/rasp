//! Shared AST, values, errors, page model, and include model for RASP.

pub mod placeholder {
    /// Placeholder for the shared ASP core model. This crate fills up as
    /// the page parser (Milestone 1) and value model are implemented.
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
