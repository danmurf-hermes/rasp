//! VBScript lexer, parser, and evaluator for RASP.

pub mod eval;
pub mod lexer;
pub mod parser;

pub use eval::exec_block;
pub use eval::exec_block_loops;
pub use eval::{ExecEnv, ResponseBuffer};
pub use lexer::Tok;
pub use parser::{Expr, Stmt, Variant, parse_block};

#[cfg(test)]
mod tests {
    use super::*;
    use asp_core::AspError;
    use std::collections::HashMap;

    fn render(source: &str) -> String {
        let stmts = parse_block(source, 1).unwrap();
        let mut env = ExecEnv::new();
        exec_block_loops(&stmts, &mut env).unwrap();
        env.response.body()
    }

    #[test]
    fn writes_literals_and_variables() {
        let out = render("Dim greeting: greeting = \"hi\": Response.Write greeting");
        assert_eq!(out, "hi");
    }

    #[test]
    fn arithmetic_and_precedence() {
        assert_eq!(render("Response.Write 2 + 3 * 4"), "14");
        assert_eq!(render("Response.Write (2 + 3) * 4"), "20");
        assert_eq!(render("Response.Write 7 / 2"), "3.5");
        assert_eq!(render("Response.Write 7 \\ 2"), "3");
    }

    #[test]
    fn string_concat() {
        let out = render("Dim n: n = 5: Response.Write \"n=\" & n");
        assert_eq!(out, "n=5");
    }

    #[test]
    fn single_line_if() {
        let src =
            "Dim x: x = 10: If x > 5 Then Response.Write \"big\" Else Response.Write \"small\"";
        assert_eq!(render(src), "big");
    }

    #[test]
    fn block_if_parses() {
        let src = "If 1 < 2 Then\n  Response.Write \"yes\"\nElse\n  Response.Write \"no\"\nEnd If";
        let stmts = parse_block(src, 1).unwrap();
        let mut env = ExecEnv::new();
        exec_block(&stmts, &mut env).unwrap();
        assert_eq!(env.response.body(), "yes");
    }

    #[test]
    fn block_if_elseif_else() {
        let src = "\
x = 3
If x = 1 Then
  Response.Write \"one\"
ElseIf x = 3 Then
  Response.Write \"three\"
Else
  Response.Write \"other\"
End If";
        let stmts = parse_block(src, 1).unwrap();
        let mut env = ExecEnv::new();
        exec_block(&stmts, &mut env).unwrap();
        assert_eq!(env.response.body(), "three");
    }

    #[test]
    fn for_next_counts() {
        let src = "Dim i, total: total = 0\nFor i = 1 To 5\n  total = total + i\nNext\nResponse.Write total";
        let stmts = parse_block(src, 1).unwrap();
        let mut env = ExecEnv::new();
        exec_block_loops(&stmts, &mut env).unwrap();
        assert_eq!(env.response.body(), "15");
    }

    #[test]
    fn for_next_negative_step() {
        let stmts = parse_block("For i = 3 To 1 Step -1: Response.Write i: Next", 1).unwrap();
        let mut env = ExecEnv::new();
        exec_block_loops(&stmts, &mut env).unwrap();
        assert_eq!(env.response.body(), "321");
    }

    #[test]
    fn do_while_loops() {
        let src = "Dim n: n = 0\nDo While n < 3\n  n = n + 1\nLoop\nResponse.Write n";
        let stmts = parse_block(src, 1).unwrap();
        let mut env = ExecEnv::new();
        exec_block_loops(&stmts, &mut env).unwrap();
        assert_eq!(env.response.body(), "3");
    }

    #[test]
    fn do_until_trailing_condition() {
        let src = "Dim n: n = 0\nDo\n  n = n + 1\nLoop Until n >= 2\nResponse.Write n";
        let stmts = parse_block(src, 1).unwrap();
        let mut env = ExecEnv::new();
        exec_block_loops(&stmts, &mut env).unwrap();
        assert_eq!(env.response.body(), "2");
    }

    #[test]
    fn session_round_trip() {
        let src = "Session(\"user\") = \"dan\"\nResponse.Write Session(\"user\")";
        let stmts = parse_block(src, 1).unwrap();
        let mut env = ExecEnv::new();
        exec_block(&stmts, &mut env).unwrap();
        assert_eq!(env.response.body(), "dan");
        assert_eq!(env.session.get("user"), Some(&Variant::Str("dan".into())));
    }

    #[test]
    fn request_read_from_querystring() {
        let mut data = HashMap::new();
        data.insert("querystring\u{0}name".to_string(), "Dan".to_string());
        let stmts = parse_block(
            "Response.Write \"Hello \" & Request.QueryString(\"name\")",
            1,
        )
        .unwrap();
        let mut env = ExecEnv::new().with_request_data(data);
        exec_block(&stmts, &mut env).unwrap();
        assert_eq!(env.response.body(), "Hello Dan");
    }

    #[test]
    fn response_end_stops_output() {
        let src = "Response.Write \"a\"\nResponse.End\nResponse.Write \"b\"";
        let stmts = parse_block(src, 1).unwrap();
        let mut env = ExecEnv::new();
        exec_block(&stmts, &mut env).unwrap();
        assert_eq!(env.response.body(), "a");
        assert!(env.response.ended);
    }

    #[test]
    fn response_clear_drops_buffer() {
        let src = "Response.Write \"old\"\nResponse.Clear\nResponse.Write \"new\"";
        let stmts = parse_block(src, 1).unwrap();
        let mut env = ExecEnv::new();
        exec_block(&stmts, &mut env).unwrap();
        assert_eq!(env.response.body(), "new");
    }

    #[test]
    fn builtins_work() {
        assert_eq!(render("Response.Write Len(\"abcd\")"), "4");
        assert_eq!(render("Response.Write UCase(\"dan\")"), "DAN");
        assert_eq!(render("Response.Write Left(\"abcdef\", 3)"), "abc");
        assert_eq!(render("Response.Write Mid(\"abcdef\", 2, 3)"), "bcd");
        assert_eq!(render("Response.Write InStr(\"abcdef\", \"cd\")"), "3");
    }

    #[test]
    fn empty_writes_as_blank() {
        let stmts = parse_block("Dim never: Response.Write \"[\" & never & \"]\"", 1).unwrap();
        let mut env = ExecEnv::new();
        exec_block(&stmts, &mut env).unwrap();
        assert_eq!(env.response.body(), "[]");
    }

    #[test]
    fn case_insensitivity_everywhere() {
        let out = render("DIM N: n = 7: response.write N");
        assert_eq!(out, "7");
    }

    #[test]
    fn unsupported_features_are_clear_errors() {
        let err = parse_block("Sub greet\nEnd Sub", 1).unwrap_err();
        assert!(matches!(err, AspError::Syntax(d) if d.message.contains("not supported")));
    }

    #[test]
    fn response_in_expression_position_is_a_clear_error() {
        let err = parse_block("Dim n: n = Response.Write", 1).unwrap_err();
        assert!(matches!(err, AspError::Syntax(d) if d.message.contains("expression position")));
    }

    #[test]
    fn division_by_zero_is_runtime_error() {
        let stmts = parse_block("Response.Write 1 / 0", 1).unwrap();
        let mut env = ExecEnv::new();
        let err = exec_block(&stmts, &mut env).unwrap_err();
        assert!(matches!(err, AspError::Runtime(d) if d.message.contains("division by zero")));
    }

    #[test]
    fn runaway_loops_are_capped() {
        let src = "Do While True\n  n = n + 1\nLoop";
        let stmts = parse_block(src, 1).unwrap();
        let mut env = ExecEnv::new();
        let err = exec_block_loops(&stmts, &mut env).unwrap_err();
        assert!(matches!(err, AspError::Runtime(d) if d.message.contains("iteration limit")));
    }

    #[test]
    fn boolean_output_format() {
        assert_eq!(render("Response.Write True"), "True");
        assert_eq!(render("Response.Write 1 = 2"), "False");
    }
}
