//! ASP intrinsic objects, request lifecycle, state, and COM mappings for RASP.

pub mod placeholder {
    /// Placeholder for the ASP runtime objects (Request, Response, Server,
    /// Session, Application). They arrive from Milestone 1 onward; see
    /// docs/asp-classic-interpreter-plan.md §7.5.
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
