//! The Classic ASP page parser: splits ASP source into literal text,
//! `<%` code blocks, `<%= expression %>` outputs, and `<!-- #include -->`
//! directives, exactly as Classic ASP preprocesses a page before the
//! script engine sees it.

use crate::error::{AspError, AspResult};

/// One preprocessed piece of an ASP page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    /// Literal HTML/text, emitted to the response exactly as written.
    Text(String),
    /// `<% ... %>` statements; the body is parsed by the language engine.
    Script { body: String },
    /// `<%= expression %>`: implicit `Response.Write expression`.
    Output { expression: String },
    /// `<!-- #include file="..." -->` or `<!-- #include virtual="..." -->`.
    /// Stored as `kind:path` (no leading slash); resolved by the assembler.
    Include { path: String },
}

impl Block {
    /// True when this block carries no executable content.
    pub fn is_literal(&self) -> bool {
        matches!(self, Block::Text(_) | Block::Output { .. })
    }
}

/// Marker alias kept from earlier drafts; new code should use [`Block`].
#[allow(dead_code)]
pub type InlineBlock = Block;

/// The script language a page asked for, or the default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LanguageSetting {
    /// `<%@ Language=VBScript %>` (name case-insensitive).
    Explicit(String),
}

/// A fully preprocessed ASP page.
#[derive(Debug, Clone)]
pub struct Page {
    /// Blocks in source order, includes resolved to paths but not yet read.
    pub blocks: Vec<Block>,
    /// `<%@ ... %>` settings collected from the page.
    pub language: LanguageSetting,
}

impl Page {
    /// Parse ASP source into a [`Page`].
    ///
    /// Include directives are rewritten to root-relative form here; the
    /// file resolution itself happens in the assembler so the parser
    /// stays filesystem-independent and trivially testable.
    pub fn parse(source: &str) -> AspResult<Self> {
        Parser::new(source).parse()
    }
}

struct Parser<'s> {
    source: &'s str,
    bytes: &'s [u8],
    /// Current line, 1-based, for diagnostics.
    line: usize,
    blocks: Vec<Block>,
    language: Option<LanguageSetting>,
}

/// Result of consuming one markup-delimited region.
#[allow(dead_code)]
enum Region {
    /// A complete block was pushed.
    Block,
    /// A `@` directive was consumed.
    AtDirective,
}

impl<'s> Parser<'s> {
    fn new(source: &'s str) -> Self {
        Self {
            source,
            bytes: source.as_bytes(),
            line: 1,
            blocks: Vec::new(),
            language: None,
        }
    }

    fn parse(mut self) -> AspResult<Page> {
        let mut text_start = 0usize;
        let at = |i: usize, b: &[u8]| {
            i + b.len() <= self.bytes.len() && &self.bytes[i..i + b.len()] == b
        };

        let mut i = 0usize;
        while i < self.bytes.len() {
            if self.bytes[i] == b'\n' {
                self.line += 1;
                i += 1;
                continue;
            }
            // Case-insensitive `<%` start.
            if at(i, b"<%") {
                let opener = self.line;
                self.push_text(text_start, i);
                i = self.parse_script(i, opener)?;
                text_start = i;
                continue;
            }
            // Case-insensitive `<!--` comment start; may be an include.
            if at(i, b"<!--")
                && let Some(end) = self.find_comment_end(i + 4)
            {
                let comment = &self.source[i + 4..end];
                match parse_include(comment, self.line)? {
                    Some(path) => {
                        self.push_text(text_start, i);
                        self.blocks.push(InlineBlock::Include { path });
                        i = end + 3;
                        text_start = i;
                        continue;
                    }
                    None => {
                        // Ordinary HTML comment: part of the page output.
                        i += 4;
                        continue;
                    }
                }
            }
            i += 1;
        }
        self.push_text(text_start, self.bytes.len());

        Ok(Page {
            blocks: self.blocks,
            language: self
                .language
                .unwrap_or(LanguageSetting::Explicit("VBScript".to_string())),
        })
    }

    /// Push the source region `text_start..end` as a text block, dropping
    /// the region if it is empty.
    fn push_text(&mut self, start: usize, end: usize) {
        if end > start {
            self.blocks
                .push(Block::Text(self.source[start..end].to_string()));
        }
    }

    /// Consume one `<%`, `<%=`, or `<%@` region starting at `start` and
    /// return the offset just past its `%>`.
    fn parse_script(&mut self, start: usize, opener_line: usize) -> AspResult<usize> {
        let after_marker = start + 2;
        let expression = after_marker < self.bytes.len() && self.bytes[after_marker] == b'=';
        let directive = after_marker < self.bytes.len() && self.bytes[after_marker] == b'@';

        let content_start = if expression || directive {
            after_marker + 1
        } else {
            after_marker
        };
        let mut i = content_start;
        while i < self.bytes.len() {
            if self.bytes[i] == b'%' && self.bytes.get(i + 1) == Some(&b'>') {
                let content_end = i;
                let content = self.source[content_start..content_end].trim();
                if content_end > content_start {
                    let newline_count = self.source[content_start..content_end]
                        .bytes()
                        .filter(|&b| b == b'\n')
                        .count();
                    self.line += newline_count;
                }
                if expression {
                    if content.is_empty() {
                        return Err(AspError::Syntax(crate::error::Diagnostic::new(
                            opener_line,
                            "<%= %> requires an expression",
                        )));
                    }
                    self.blocks.push(Block::Output {
                        expression: content.to_string(),
                    });
                } else if directive {
                    self.parse_directive(content, opener_line)?;
                } else {
                    self.blocks.push(Block::Script {
                        body: content.to_string(),
                    });
                }
                return Ok(i + 2);
            }
            if self.bytes[i] == b'\n' {
                self.line += 1;
            }
            i += 1;
        }
        Err(AspError::Syntax(crate::error::Diagnostic::new(
            opener_line,
            "unterminated <% block: missing %>",
        )))
    }

    /// Parse the body of a `<%@ ... %>` directive. Only Language is known.
    fn parse_directive(&mut self, body: &str, line: usize) -> AspResult<()> {
        let normalized = body.replace(['=', '"'], " ");
        let words = normalized.split_whitespace().collect::<Vec<_>>();
        for i in 0..words.len() {
            if words[i].eq_ignore_ascii_case("Language")
                && let Some(lang) = words.get(i + 1)
            {
                self.language = Some(LanguageSetting::Explicit(lang.to_string()));
                return Ok(());
            }
        }
        // Unknown directives (e.g. CodePage) are tolerated for now.
        let _ = line;
        Ok(())
    }

    /// Find the end of a `-->` starting the search at `from`.
    /// Returns the byte offset of the `-` that begins `-->`.
    fn find_comment_end(&self, from: usize) -> Option<usize> {
        if from + 3 > self.bytes.len() {
            return None;
        }
        for i in from..=self.bytes.len() - 3 {
            if &self.bytes[i..i + 3] == b"--\x3e" {
                return Some(i);
            }
        }
        None
    }
}

/// Recognize a comment body as `#include file="..."` or
/// `#include virtual="..."`, returning the path when it matches.
/// Anything else is an ordinary HTML comment and returns `None`.
fn parse_include(body: &str, line: usize) -> AspResult<Option<String>> {
    let trimmed = body.trim();
    let is_include_directive = trimmed
        .get(..8)
        .map(|head| head.eq_ignore_ascii_case("#include"))
        .unwrap_or(false);
    if !is_include_directive {
        return Ok(None);
    }
    let after = trimmed[8..].trim_start();
    let kind_end = after
        .find(|c: char| !c.is_ascii_alphanumeric())
        .unwrap_or(after.len());
    let kind_lower = after[..kind_end].to_ascii_lowercase();
    if kind_lower != "file" && kind_lower != "virtual" {
        return Err(AspError::Syntax(crate::error::Diagnostic::new(
            line,
            format!("unknown include type: {}", after.trim_end()),
        )));
    }
    let rest = after[kind_end..].trim_start();
    let rest = rest.strip_prefix('=').map(str::trim_start).unwrap_or(rest);
    let path = strip_quotes(rest).ok_or_else(|| {
        AspError::Syntax(crate::error::Diagnostic::new(
            line,
            "include directive requires a quoted path",
        ))
    })?;
    Ok(Some(format!("{kind_lower}:{path}")))
}

fn strip_quotes(rest: &str) -> Option<String> {
    let s = rest.trim();
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        Some(s[1..s.len() - 1].to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(source: &str) -> Vec<Block> {
        Page::parse(source).unwrap().blocks
    }

    #[test]
    fn plain_text_is_one_block() {
        assert_eq!(
            parse("<h1>hi</h1>"),
            vec![Block::Text("<h1>hi</h1>".into())]
        );
    }

    #[test]
    fn script_and_output_blocks() {
        let blocks = parse("a<% x=1 %>b<%= x %>c");
        assert_eq!(
            blocks,
            vec![
                Block::Text("a".into()),
                Block::Script { body: "x=1".into() },
                Block::Text("b".into()),
                Block::Output {
                    expression: "x".into()
                },
                Block::Text("c".into()),
            ]
        );
    }

    #[test]
    fn case_insensitive_tags_and_trimmed_expression() {
        let blocks = parse("a<%= \"hi\" %>b<% RESPONSE.WRITE \"x\" %>");
        assert_eq!(
            blocks[1],
            Block::Output {
                expression: "\"hi\"".into()
            }
        );
        assert_eq!(
            blocks[3],
            Block::Script {
                body: "RESPONSE.WRITE \"x\"".into()
            }
        );
    }

    #[test]
    fn directive_language_is_read_case_insensitively() {
        let page = Page::parse("<%@ LANGUAGE=\"vbscript\" %><%= 1 %>").unwrap();
        assert_eq!(page.language, LanguageSetting::Explicit("vbscript".into()));
    }

    #[test]
    fn ordinary_comments_pass_through_to_output() {
        assert_eq!(
            parse("a<!-- hello -->b"),
            vec![Block::Text("a<!-- hello -->b".into())]
        );
    }

    #[test]
    fn file_include_is_parsed() {
        let blocks = parse("a<!-- #include file=\"f.asp\" -->b");
        assert_eq!(
            blocks,
            vec![
                Block::Text("a".into()),
                Block::Include {
                    path: "file:f.asp".into()
                },
                Block::Text("b".into()),
            ]
        );
    }

    #[test]
    fn virtual_include_is_parsed() {
        let blocks = parse("<!-- #include virtual=\"/inc/a.asp\" -->");
        assert_eq!(
            blocks[0],
            Block::Include {
                path: "virtual:/inc/a.asp".into()
            }
        );
    }

    #[test]
    fn empty_expression_is_a_syntax_error() {
        let err = Page::parse("a<%= %>b").unwrap_err();
        assert!(matches!(err, AspError::Syntax(d) if d.line == 1));
    }

    #[test]
    fn unterminated_block_is_a_syntax_error() {
        let err = Page::parse("a<% x = 1\nb").unwrap_err();
        assert!(matches!(err, AspError::Syntax(d) if d.line == 1));
    }

    #[test]
    fn unknown_include_type_is_reported() {
        let err = Page::parse("<!-- #include database=\"x\" -->").unwrap_err();
        assert!(matches!(err, AspError::Syntax(_)));
    }

    #[test]
    fn multi_line_script_counts_lines() {
        let err = Page::parse("a\n<%\nx = 1").unwrap_err();
        assert!(matches!(err, AspError::Syntax(d) if d.line == 2));
    }
}
