//! Shared error and diagnostic types for RASP.

use std::fmt;

/// Where in an ASP source a problem occurred.
///
/// Line numbers are 1-based and refer to the page source after includes
/// have been resolved, which matches what a user sees in their file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    /// 1-based line number in the assembled page source.
    pub line: usize,
    /// Human-readable explanation.
    pub message: String,
}

impl Diagnostic {
    pub fn new(line: usize, message: impl Into<String>) -> Self {
        Self {
            line,
            message: message.into(),
        }
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

/// Errors produced while loading, parsing, or rendering an ASP page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AspError {
    /// The page could not be parsed.
    Syntax(Diagnostic),
    /// The page parsed but failed while it was being executed.
    Runtime(Diagnostic),
    /// The requested page does not exist inside the application root.
    PageNotFound(String),
    /// A page or include path tried to escape the application root.
    PathEscape(String),
    /// A referenced include file does not exist.
    IncludeNotFound(String),
    /// Includes form a cycle (a file appears twice in the active chain).
    IncludeCycle(String),
    /// The page names a script language RASP does not support yet.
    UnsupportedLanguage(String),
    /// Anything else that went wrong with the filesystem.
    Io(String),
}

impl fmt::Display for AspError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AspError::Syntax(d) => write!(f, "syntax error: {d}"),
            AspError::Runtime(d) => write!(f, "runtime error: {d}"),
            AspError::PageNotFound(p) => write!(f, "page not found: {p}"),
            AspError::PathEscape(p) => {
                write!(f, "path escapes the application root: {p}")
            }
            AspError::IncludeNotFound(p) => write!(f, "include not found: {p}"),
            AspError::IncludeCycle(p) => write!(f, "include cycle detected at: {p}"),
            AspError::UnsupportedLanguage(name) => {
                write!(f, "unsupported script language: {name}")
            }
            AspError::Io(m) => write!(f, "io error: {m}"),
        }
    }
}

impl std::error::Error for AspError {}

pub type AspResult<T> = Result<T, AspError>;
