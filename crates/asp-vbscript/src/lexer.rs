//! Hand-written lexer for the classic ASP subset of VBScript.
//!
//! VBScript is line-oriented and case-insensitive: keywords and
//! identifiers match without regard to case, string literals always
//! double `""` to embed a quote, comments run from `'` to end of line,
//! and a trailing `_` continues the statement onto the next line.
//! Number literals accept decimal, `&h` hex, and `&o` octal forms.

use asp_core::{AspError, AspResult};

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    /// Integer literal (decimal, `&h`/`&H` hex, `&o`/`&O` octal).
    Int(i64),
    /// Floating-point literal (`1.5`, `.5`, `2e3`).
    Float(f64),
    /// String literal with embedded-quote unescaping already applied.
    Str(String),
    /// Identifier or keyword, normalised to ASCII lower case.
    Name(String),
    /// Punctuation or operator.
    Sym(String),
    /// End of a statement line (`\n`, or a continued line's final part).
    LineEnd,
}

impl Tok {
    /// The token text as it should appear inside diagnostics.
    pub fn describe(&self) -> String {
        match self {
            Tok::Int(i) => i.to_string(),
            Tok::Float(f) => f.to_string(),
            Tok::Str(s) => format!("\"{s}\""),
            Tok::Name(n) => n.clone(),
            Tok::Sym(s) => s.clone(),
            Tok::LineEnd => "end of statement".to_string(),
        }
    }
}

/// True for VBScript reserved words the M1 grammar reserves.
pub fn is_reserved(name: &str) -> bool {
    const RESERVED: [&str; 30] = [
        "if", "then", "else", "elseif", "end", "for", "to", "next", "do", "while", "until", "loop",
        "dim", "redim", "const", "sub", "function", "exit", "set", "call", "on", "error", "resume",
        "class", "with", "select", "case", "not", "and", "or",
    ];
    let name = name.to_ascii_lowercase();
    RESERVED.contains(&name.as_str())
}

/// Tokenize one `<%` block body. The body is lexed as a whole; statement
/// separation happens in the parser, which consumes `LineEnd` between
/// statements.
pub fn lex(body: &str, base_line: usize) -> AspResult<Vec<Tok>> {
    let chars: Vec<char> = body.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0usize;
    let mut line = base_line;

    while i < chars.len() {
        let c = chars[i];
        match c {
            '\n' => {
                line += 1;
                tokens.push(Tok::LineEnd);
                i += 1;
            }
            ' ' | '\t' | '\r' => i += 1,
            '\'' => {
                // Comment to end of line; the newline itself is emitted
                // as LineEnd by the general case on the next pass.
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            '"' => {
                let (tok, next) = lex_string(&chars, i, line)?;
                tokens.push(tok);
                i = next;
            }
            '0'..='9' => {
                let (tok, next) = lex_number(&chars, i, line)?;
                tokens.push(tok);
                i = next;
            }
            '.' => {
                // `.` only starts a float when a digit follows.
                if matches!(chars.get(i + 1), Some('0'..='9')) {
                    let (tok, next) = lex_number(&chars, i, line)?;
                    tokens.push(tok);
                    i = next;
                } else {
                    tokens.push(Tok::Sym(".".to_string()));
                    i += 1;
                }
            }
            '&' | 'o' | 'O' if starts_radix(&chars, i, c) => {
                let (tok, next) = lex_radix(&chars, i, line)?;
                tokens.push(tok);
                i = next;
            }
            '&' => {
                tokens.push(Tok::Sym("&".to_string()));
                i += 1;
            }
            '_' => {
                // Line continuation: skip `_`, horizontal space, and at
                // most one newline so the statement spans two lines.
                i += 1;
                while matches!(chars.get(i), Some(' ') | Some('\t') | Some('\r')) {
                    i += 1;
                }
                if matches!(chars.get(i), Some('\n')) {
                    line += 1;
                    i += 1;
                }
            }
            ':' => {
                tokens.push(Tok::LineEnd);
                i += 1;
            }
            '<' => {
                if matches!(chars.get(i + 1), Some('>')) {
                    tokens.push(Tok::Sym("<>".to_string()));
                    i += 2;
                } else if matches!(chars.get(i + 1), Some('=')) {
                    tokens.push(Tok::Sym("<=".to_string()));
                    i += 2;
                } else {
                    tokens.push(Tok::Sym("<".to_string()));
                    i += 1;
                }
            }
            '>' => {
                if matches!(chars.get(i + 1), Some('=')) {
                    tokens.push(Tok::Sym(">=".to_string()));
                    i += 2;
                } else {
                    tokens.push(Tok::Sym(">".to_string()));
                    i += 1;
                }
            }
            '=' | '+' | '-' | '*' | '/' | '\\' | '^' | '(' | ')' | ',' => {
                tokens.push(Tok::Sym(c.to_string()));
                i += 1;
            }
            c if c.is_ascii_alphabetic() => {
                let start = i;
                while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
                let word: String = chars[start..i].iter().collect();
                tokens.push(Tok::Name(word.to_ascii_lowercase()));
            }
            other => {
                return Err(AspError::Syntax(asp_core::Diagnostic::new(
                    line,
                    format!("unexpected character {other:?}"),
                )));
            }
        }
    }
    tokens.push(Tok::LineEnd);
    Ok(tokens)
}

/// Lex a flattened, delimiter-joined statement stream where the Classic
/// ASP text/`<%= %>` between `<% %>` blocks must re-enter the script.
/// This is the M2-style flattening path (see asp-runtime `flatten`).
pub fn lex_flattened(parts: &[String], base_line: usize) -> AspResult<Vec<Tok>> {
    let joined = parts.join("\n");
    lex(&joined, base_line)
}

/// `&h`/`&o` radix literals begin at the `&`; bare `o`/`O` forms exist in
/// some dialects but are rejected here to stay explicit.
fn starts_radix(chars: &[char], i: usize, c: char) -> bool {
    if c != '&' {
        return false;
    }
    matches!(
        chars.get(i + 1),
        Some('h') | Some('H') | Some('o') | Some('O')
    )
}

fn lex_radix(chars: &[char], start: usize, line: usize) -> AspResult<(Tok, usize)> {
    let radix = match chars.get(start + 1) {
        Some('h') | Some('H') => 16u32,
        _ => 8u32,
    };
    let digits_start = start + 2;
    let mut end = digits_start;
    while end < chars.len() && (chars[end].is_ascii_alphanumeric() || chars[end] == '&') {
        end += 1;
    }
    let digits: String = chars[digits_start..end]
        .iter()
        .filter(|c| **c != '&')
        .collect();
    let value = i64::from_str_radix(&digits, radix).map_err(|_| {
        AspError::Syntax(asp_core::Diagnostic::new(
            line,
            format!("invalid base-{radix} literal: {digits}"),
        ))
    })?;
    Ok((Tok::Int(value), end))
}

fn lex_number(chars: &[char], start: usize, line: usize) -> AspResult<(Tok, usize)> {
    let mut end = start;
    let mut seen_dot = false;
    let mut seen_exp = false;
    while end < chars.len() {
        let c = chars[end];
        if c.is_ascii_digit() {
            end += 1;
        } else if c == '.' && !seen_dot && !seen_exp {
            seen_dot = true;
            end += 1;
        } else if (c == 'e' || c == 'E')
            && !seen_exp
            && matches!(chars.get(end + 1), Some('0'..='9') | Some('+') | Some('-'))
        {
            seen_exp = true;
            end += 2;
        } else {
            break;
        }
    }
    let text: String = chars[start..end].iter().collect();
    let tok = if seen_dot || seen_exp {
        Tok::Float(text.parse::<f64>().map_err(|_| {
            AspError::Syntax(asp_core::Diagnostic::new(
                line,
                format!("bad number: {text}"),
            ))
        })?)
    } else {
        Tok::Int(text.parse::<i64>().map_err(|_| {
            AspError::Syntax(asp_core::Diagnostic::new(
                line,
                format!("bad number: {text}"),
            ))
        })?)
    };
    Ok((tok, end))
}

fn lex_string(chars: &[char], start: usize, mut line: usize) -> AspResult<(Tok, usize)> {
    let mut value = String::new();
    let mut i = start + 1;
    while i < chars.len() {
        match chars[i] {
            '"' => {
                if matches!(chars.get(i + 1), Some('"')) {
                    value.push('"');
                    i += 2;
                } else {
                    return Ok((Tok::Str(value), i + 1));
                }
            }
            '\n' => {
                line += 1;
                i += 1;
            }
            c => {
                value.push(c);
                i += 1;
            }
        }
    }
    Err(AspError::Syntax(asp_core::Diagnostic::new(
        line,
        "unterminated string literal",
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lex_ok(src: &str) -> Vec<Tok> {
        lex(src, 7).unwrap()
    }

    #[test]
    fn numbers_dec_hex_oct() {
        assert_eq!(lex_ok("1 &hff &o17")[0], Tok::Int(1));
        assert_eq!(find_int(&lex_ok("1 &hff &o17"), 1), 255);
        assert_eq!(find_int(&lex_ok("1 &hff &o17"), 2), 15);
    }

    fn find_int(tokens: &[Tok], idx: usize) -> i64 {
        match &tokens[idx] {
            Tok::Int(i) => *i,
            other => panic!("expected int, got {other:?}"),
        }
    }

    #[test]
    fn floats_and_strings() {
        assert!(matches!(lex_ok("2.5")[0], Tok::Float(f) if (f - 2.5).abs() < 1e-9));
        assert_eq!(
            lex_ok("\"say \"\"hi\"\"\"")[0],
            Tok::Str("say \"hi\"".into())
        );
    }

    #[test]
    fn names_lowercase_and_symbols() {
        let tokens = lex_ok("Response.Write");
        assert_eq!(tokens[0], Tok::Name("response".into()));
        assert_eq!(tokens[1], Tok::Sym(".".into()));
        assert_eq!(tokens[2], Tok::Name("write".into()));
    }

    #[test]
    fn operators() {
        let tokens = lex_ok("<> <= >=");
        assert_eq!(
            tokens[..3],
            [
                Tok::Sym("<>".into()),
                Tok::Sym("<=".into()),
                Tok::Sym(">=".into())
            ]
        );
    }

    #[test]
    fn comments_and_continuations() {
        assert!(lex("' note\n", 1).is_ok());
        assert!(lex("x = _\n 1", 1).is_ok());
    }

    #[test]
    fn bad_character_is_error() {
        let err = lex("x @ y", 3).unwrap_err();
        assert!(matches!(err, AspError::Syntax(d) if d.line == 3));
    }
}
