//! VBScript lexer, parser, and evaluator for RASP.

pub mod ado;
pub mod eval;
pub mod lexer;
pub mod native;
pub mod parser;
pub mod vb_datetime;

pub use eval::{
    ApplicationState, Cookie, DEFAULT_SESSION_TIMEOUT_MIN, ExecEnv, ResponseBuffer, SessionStore,
    StateStores, exec_block, exec_block_loops,
};
pub use lexer::Tok;
pub use native::{NativeHost, NativeObj, NativeState, SubPage, create_native};
pub use parser::{Expr, Stmt, Variant, parse_block};

#[cfg(test)]
mod tests {
    use super::*;
    use asp_core::AspError;
    use std::collections::HashMap;

    fn render(src: &str) -> String {
        let stmts = parse_block(src, 1).unwrap();
        let mut env = ExecEnv::new();
        exec_block_loops(&stmts, &mut env).unwrap();
        env.response.body()
    }

    fn render_err(src: &str) -> AspError {
        let stmts = parse_block(src, 1).unwrap();
        let mut env = ExecEnv::new();
        exec_block_loops(&stmts, &mut env).unwrap_err()
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
        assert_eq!(render(src), "yes");
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
        assert_eq!(render(src), "three");
    }

    #[test]
    fn for_next_counts() {
        let src = "Dim i, total: total = 0\nFor i = 1 To 5\n  total = total + i\nNext\nResponse.Write total";
        assert_eq!(render(src), "15");
    }

    #[test]
    fn for_next_negative_step() {
        assert_eq!(
            render("For i = 3 To 1 Step -1: Response.Write i: Next"),
            "321"
        );
    }

    #[test]
    fn nested_for_loops() {
        let src = "For i = 1 To 2\nFor j = 1 To 2\nResponse.Write i & j\nNext\nNext";
        assert_eq!(render(src), "11122122");
    }

    #[test]
    fn for_inside_do() {
        let src = "n = 0\nDo While n < 2\nFor i = 1 To 2\nResponse.Write i\nNext\nn = n + 1\nLoop";
        assert_eq!(render(src), "1212");
    }

    #[test]
    fn exit_for_and_exit_do() {
        let src = "For i = 1 To 10\nIf i = 3 Then Exit For\nResponse.Write i\nNext";
        assert_eq!(render(src), "12");
        let src = "n = 0\nDo While True\nn = n + 1\nIf n >= 2 Then Exit Do\nLoop\nResponse.Write n";
        assert_eq!(render(src), "2");
    }

    #[test]
    fn do_while_loops() {
        let src = "Dim n: n = 0\nDo While n < 3\n  n = n + 1\nLoop\nResponse.Write n";
        assert_eq!(render(src), "3");
    }

    #[test]
    fn do_until_trailing_condition() {
        let src = "Dim n: n = 0\nDo\n  n = n + 1\nLoop Until n >= 2\nResponse.Write n";
        assert_eq!(render(src), "2");
    }

    #[test]
    fn arrays_fixed_size_and_indexed() {
        let src =
            "Dim a(2)\na(0) = \"x\"\na(1) = 5\na(2) = a(1) + 1\nResponse.Write a(0) & a(1) & a(2)";
        assert_eq!(render(src), "x56");
    }

    #[test]
    fn array_literal_and_ubound() {
        let src = "names = Array(\"ann\", \"bob\", \"cid\")\nResponse.Write UBound(names) & \":\" & names(1)";
        assert_eq!(render(src), "2:bob");
    }

    #[test]
    fn split_and_join() {
        let src = "parts = Split(\"a,b,c\", \",\")\nResponse.Write UBound(parts) & Join(parts, \"-\") & parts(2)";
        assert_eq!(render(src), "2a-b-cc");
    }

    #[test]
    fn array_index_out_of_range_is_runtime_error() {
        let err = render_err("Dim a(2): a(5) = 1");
        assert!(matches!(err, AspError::Runtime(d) if d.message.contains("out of range")));
    }

    #[test]
    fn sub_call_before_and_after_definition() {
        let src = "greet \"Dan\"\nSub greet(who)\n  Response.Write \"hi \" & who\nEnd Sub\ngreet \"Claire\"";
        assert_eq!(render(src), "hi Danhi Claire");
    }

    #[test]
    fn function_returns_and_exits() {
        let src = "Response.Write double(21)\nFunction double(n)\n  double = n * 2\nEnd Function";
        assert_eq!(render(src), "42");
    }

    #[test]
    fn byref_writeback_and_byval_isolation() {
        let src = "n = 5\nbump n\nResponse.Write n\nm = 7\nCall bump(m)\nResponse.Write m\nSub bump(x)\n  x = x + 1\nEnd Sub";
        assert_eq!(render(src), "68");
    }

    #[test]
    fn recursion_within_cap() {
        let src = "Response.Write fact(5)\nFunction fact(n)\n  If n <= 1 Then\n    fact = 1\n  Else\n    fact = n * fact(n - 1)\n  End If\nEnd Function";
        assert_eq!(render(src), "120");
    }

    #[test]
    fn function_return_via_name_assignment() {
        let src = "Function pick(flag)\n  If flag Then\n    pick = \"yes\"\n  Else\n    pick = \"no\"\n  End If\nEnd Function\nResponse.Write pick(True) & pick(False)";
        assert_eq!(render(src), "yesno");
    }

    #[test]
    fn exit_function_skips_rest() {
        let src = "Function half(n)\n  If n > 100 Then\n    half = 99\n    Exit Function\n  End If\n  half = n / 2\nEnd Function\nResponse.Write half(200) & half(10)";
        assert_eq!(render(src), "995");
    }

    #[test]
    fn wrong_argument_count_is_clear() {
        let err = render_err("Function f(a)\nEnd Function\nf 1, 2");
        assert!(matches!(err, AspError::Runtime(d) if d.message.contains("expects 1 argument")));
    }

    #[test]
    fn unbalanced_delimiters_are_clear() {
        let e1 = render_err("For i = 1 To 3\nx = 1");
        assert!(matches!(e1, AspError::Runtime(d) if d.message.contains("missing 'Next'")));
        let e2 = render_err("Next");
        assert!(matches!(e2, AspError::Runtime(d) if d.message.contains("matching 'For'")));
        let e3 = render_err("Do While True\nx = 1");
        assert!(matches!(e3, AspError::Runtime(d) if d.message.contains("missing 'Loop'")));
    }

    #[test]
    fn unbalanced_procedure_is_clear() {
        let err = render_err("Sub s()\nResponse.Write 1");
        assert!(matches!(err, AspError::Runtime(d) if d.message.contains("missing 'End Sub'")));
    }

    #[test]
    fn cross_block_loop_pairs_through_delimiters() {
        // Simulates flattening: For in one block, Next in a later one.
        let src = "For i = 1 To 2\nResponse.Write \"[\" & i\nNext";
        let stmts = parse_block(src, 1).unwrap();
        assert!(matches!(stmts[0], Stmt::ForLoopOpen { .. }));
        let mut env = ExecEnv::new();
        exec_block_loops(&stmts, &mut env).unwrap();
        assert_eq!(env.response.body(), "[1[2");
    }

    #[test]
    fn session_round_trip() {
        let src = "Session(\"user\") = \"dan\"\nResponse.Write Session(\"user\")";
        let stmts = parse_block(src, 1).unwrap();
        let mut env = ExecEnv::new();
        exec_block(&stmts, &mut env).unwrap();
        assert_eq!(env.response.body(), "dan");
        assert_eq!(
            env.session.values.get("user"),
            Some(&Variant::Str("dan".into()))
        );
    }

    #[test]
    fn session_contents_round_trip() {
        let out = render(
            "Session.Contents(\"page\") = \"home\"\nResponse.Write Session.Contents(\"page\")",
        );
        assert_eq!(out, "home");
    }

    #[test]
    fn application_round_trip() {
        let src = "Application(\"hits\") = 41\nApplication(\"hits\") = Application(\"hits\") + 1\nResponse.Write Application(\"hits\")";
        let stmts = parse_block(src, 1).unwrap();
        let mut env = ExecEnv::new();
        exec_block(&stmts, &mut env).unwrap();
        assert_eq!(env.response.body(), "42");
        assert_eq!(env.application.values.get("hits"), Some(&Variant::Int(42)));
    }

    #[test]
    fn session_id_renders_hex() {
        let src = "Response.Write Session.SessionID";
        let stmts = parse_block(src, 1).unwrap();
        let mut env = ExecEnv::new();
        env.session.id = 0x0a0b0c;
        exec_block(&stmts, &mut env).unwrap();
        assert_eq!(env.response.body(), "a0b0c");
    }

    #[test]
    fn session_timeout_round_trip() {
        let src = "Session.Timeout = 45\nResponse.Write \"ok\"";
        let stmts = parse_block(src, 1).unwrap();
        let mut env = ExecEnv::new();
        exec_block(&stmts, &mut env).unwrap();
        assert_eq!(env.response.body(), "ok");
        assert_eq!(env.session.timeout_min, 45);
    }

    #[test]
    fn session_timeout_minimum_is_enforced() {
        let err = render_err("Session.Timeout = 0");
        assert!(matches!(err, AspError::Runtime(d) if d.message.contains("at least 1")));
    }

    #[test]
    fn session_abandon_flags_the_store() {
        let src = "Session(\"keep\") = \"no\"\nSession.Abandon";
        let stmts = parse_block(src, 1).unwrap();
        let mut env = ExecEnv::new();
        exec_block(&stmts, &mut env).unwrap();
        assert!(env.session.abandoned);
    }

    #[test]
    fn application_lock_unlock_flags() {
        let src = "Application.Lock\nApplication(\"safe\") = True\nApplication.UnLock";
        let stmts = parse_block(src, 1).unwrap();
        let mut env = ExecEnv::new();
        exec_block(&stmts, &mut env).unwrap();
        assert!(!env.application.locked);
        assert_eq!(
            env.application.values.get("safe"),
            Some(&Variant::Bool(true))
        );
    }

    #[test]
    fn session_member_unknown_is_clear() {
        let err = parse_block("Session.Count", 1).unwrap_err();
        assert!(
            matches!(err, AspError::Syntax(d) if d.message.contains("not supported in Milestone 4"))
        );
        let err = parse_block("Application.StaticObjects(1)", 1).unwrap_err();
        assert!(matches!(err, AspError::Syntax(d) if d.message.contains("StaticObjects")));
        let err = parse_block("Session.Timeout = 1\nx = Session.Timeout", 1).unwrap_err();
        assert!(
            matches!(err, AspError::Syntax(d) if d.message.to_ascii_lowercase().contains("session.timeout"))
        );
    }

    #[test]
    fn state_stores_flow_through_the_env() {
        let src = "Response.Write Session(\"who\") & \"/\" & Application(\"app\")";
        let stmts = parse_block(src, 1).unwrap();
        let mut env = ExecEnv::new().with_state(StateStores {
            session: SessionStore::new(7),
            application: ApplicationState {
                values: HashMap::from([("app".to_string(), Variant::Str("v".into()))]),
                locked: false,
                started: false,
            },
        });
        exec_block(&stmts, &mut env).unwrap();
        assert_eq!(env.response.body(), "/v"); // Session 7 has no "who" yet
        assert_eq!(env.session.id, 7);
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
        assert_eq!(render(src), "new");
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
        assert_eq!(
            render("Dim never: Response.Write \"[\" & never & \"]\""),
            "[]"
        );
    }

    #[test]
    fn case_insensitivity_everywhere() {
        let out = render("DIM N: n = 7: response.write N");
        assert_eq!(out, "7");
    }

    #[test]
    fn procedure_calls_case_insensitive() {
        let out = render("GREET \"x\"\nSUB GREET(w)\nResponse.Write W & w\nEND SUB");
        assert_eq!(out, "xx");
    }

    #[test]
    fn unsupported_features_are_clear_errors() {
        // Unknown ProgID is a RUNTIME error now (registry lookup fails).
        let err = render_err("Set x = Server.CreateObject(\"Thing\")");
        assert!(matches!(err, AspError::Runtime(d) if d.message.contains("not available")));
        let err = parse_block("On Error Resume Next", 1).unwrap_err();
        assert!(matches!(err, AspError::Syntax(d) if d.message.contains("not supported")));
        let err = parse_block("ReDim a(5)", 1).unwrap_err();
        assert!(matches!(err, AspError::Syntax(d) if d.message.contains("not supported")));
    }

    #[test]
    fn dictionary_round_trip() {
        let out = render(
            "Set d = Server.CreateObject(\"Scripting.Dictionary\")\n\
             d.Add \"name\", \"dan\"\n\
             d.Add \"age\", 43\n\
             Response.Write d(\"name\") & \"/\" & d.Item(\"age\") & \"/\" & d.Count",
        );
        assert_eq!(out, "dan/43/2");
    }

    #[test]
    fn dictionary_exists_remove_and_errors() {
        let out = render(
            "Set d = Server.CreateObject(\"Scripting.Dictionary\")\n\
             d.Add \"a\", 1\n\
             Response.Write d.Exists(\"a\") & \"\" & d.Exists(\"b\")",
        );
        assert_eq!(out, "TrueFalse");
        let err = render_err(
            "Set d = Server.CreateObject(\"Scripting.Dictionary\")\n d.Add \"a\", 1\n d.Add \"a\", 2",
        );
        assert!(matches!(err, AspError::Runtime(d) if d.message.contains("already associated")));
        let err = render_err(
            "Set d = Server.CreateObject(\"Scripting.Dictionary\")\n Response.Write d(\"nope\")",
        );
        assert!(matches!(err, AspError::Runtime(d) if d.message.contains("Element not found")));
        let err = render_err(
            "Set d = Server.CreateObject(\"Scripting.Dictionary\")\n d.Remove(\"nope\")",
        );
        assert!(matches!(err, AspError::Runtime(d) if d.message.contains("Element not found")));
    }

    #[test]
    fn native_objects_share_reference_semantics() {
        let out = render(
            "Set a = Server.CreateObject(\"Scripting.Dictionary\")\n\
             Set b = a\n\
             b.Add \"k\", \"v\"\n\
             Response.Write a(\"k\")",
        );
        assert_eq!(out, "v");
    }

    #[test]
    fn fso_without_host_is_a_clear_error() {
        let err = render_err(
            "Set f = Server.CreateObject(\"Scripting.FileSystemObject\")\n Response.Write f.FileExists(\"x.txt\")",
        );
        assert!(matches!(err, AspError::Runtime(d) if d.message.contains("needs a host")));
    }

    #[test]
    fn division_by_zero_is_runtime_error() {
        let err = render_err("Response.Write 1 / 0");
        assert!(matches!(err, AspError::Runtime(d) if d.message.contains("division by zero")));
    }

    #[test]
    fn runaway_loops_are_capped() {
        let err = render_err("Do While True\n  n = n + 1\nLoop");
        assert!(matches!(err, AspError::Runtime(d) if d.message.contains("iteration limit")));
    }

    #[test]
    fn runaway_recursion_is_capped() {
        let err = render_err("Function r()\n  r = r()\nEnd Function\nr");
        assert!(
            matches!(err, AspError::Runtime(d) if d.message.contains("depth") || d.message.contains("argument"))
        );
    }

    #[test]
    fn conversions_bankers_rounding() {
        assert_eq!(
            render("Response.Write CInt(2.5) & CInt(3.5) & CInt(0.5) & CInt(1.5)"),
            "2402"
        );
        assert_eq!(render("Response.Write CInt(\"123\")"), "123");
        assert_eq!(render("Response.Write CLng(2147483647)"), "2147483647");
    }

    #[test]
    fn conversion_overflow_is_error() {
        let err = render_err("Response.Write CInt(40000)");
        assert!(matches!(err, AspError::Runtime(d) if d.message.contains("overflow")));
    }

    #[test]
    fn date_builtins() {
        assert_eq!(render("Response.Write Year(\"2026-09-30\")"), "2026");
        assert_eq!(render("Response.Write Month(\"9/30/2026\")"), "9");
        assert_eq!(render("Response.Write Day(\"2026-09-30\")"), "30");
        assert_eq!(render("Response.Write Hour(\"2026-09-30 14:30:00\")"), "14");
        assert_eq!(render("Response.Write Minute(\"14:30\")"), "30");
        assert_eq!(render("Response.Write Second(\"14:30:07\")"), "7");
        // 2026-09-30 is a Wednesday (Sunday=1 numbering -> 4).
        assert_eq!(render("Response.Write Weekday(\"2026-09-30\")"), "4");
        assert_eq!(
            render("Response.Write DateSerial(2026, 9, 30)"),
            "2026-09-30"
        );
        assert_eq!(
            render("Response.Write DateSerial(2026, 13, 1)"),
            "2027-01-01"
        );
        assert_eq!(
            render("Response.Write DateSerial(2026, 2, 29)"),
            "2026-03-01"
        );
        assert_eq!(
            render("Response.Write DateAdd(\"yyyy\", 1, \"2026-02-28\")"),
            "2027-02-28"
        );
        assert_eq!(
            render("Response.Write DateAdd(\"m\", 1, \"2026-01-31\")"),
            "2026-02-28"
        );
        assert_eq!(
            render("Response.Write DateDiff(\"d\", \"2026-09-29\", \"2026-09-30\")"),
            "1"
        );
        assert_eq!(
            render("Response.Write DateDiff(\"m\", \"2026-01-15\", \"2026-03-01\")"),
            "2"
        );
        assert_eq!(render("Response.Write CDate(\"9/30/2026\")"), "2026-09-30");
        assert_eq!(
            render("Response.Write DateValue(\"2026-09-30 08:00\")"),
            "2026-09-30"
        );
    }

    #[test]
    fn date_variable_arithmetic() {
        let src = "d = CDate(\"2026-09-30\")\ne = d + 2\nResponse.Write e";
        assert_eq!(render(src), "2026-10-02");
        let src = "Response.Write DateDiff(\"d\", \"2026-09-30\", \"2026-10-05\")";
        assert_eq!(render(src), "5");
    }

    #[test]
    fn is_functions() {
        assert_eq!(
            render("Response.Write IsArray(Array(1)) & IsDate(\"x\") & IsDate(\"9/30/2026\")"),
            "TrueFalseTrue"
        );
        assert_eq!(
            render("Response.Write IsEmpty(x) & IsNumeric(72)"),
            "TrueTrue"
        );
    }

    #[test]
    fn boolean_output_format() {
        assert_eq!(render("Response.Write True"), "True");
        assert_eq!(render("Response.Write 1 = 2"), "False");
    }
}
