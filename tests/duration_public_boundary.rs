use mdbase::expressions::evaluator::{evaluate, EvalContext};
use mdbase::expressions::parser::Parser;
use serde_json::Value;

fn eval(source: &str) -> Result<Value, mdbase::expressions::evaluator::EvalError> {
    let expression = Parser::parse(source).expect("public parser");
    evaluate(&expression, &EvalContext::empty())
}

#[test]
fn public_parser_and_evaluator_dispatch_date_datetime_and_offset_forms() {
    for (expression, expected) in [
        (r#""2026-01-31" + "1M""#, "2026-02-28"),
        (r#""2026-01-31T10:20:30" + "1M""#, "2026-02-28T10:20:30"),
        // Existing contract intentionally drops the parsed RFC3339 offset.
        (
            r#""2026-01-31T10:20:30+05:30" + "1M""#,
            "2026-02-28T10:20:30",
        ),
        (r#""2026-03-31" - "1M""#, "2026-02-28"),
        (r#""2026-03-31" - "-1M""#, "2026-04-30"),
        (r#""2026-03-31" - "1d""#, "2026-03-30"),
        (r#""2026-03-31" - "-1d""#, "2026-04-01"),
    ] {
        assert_eq!(
            eval(expression).unwrap(),
            Value::String(expected.into()),
            "{expression}"
        );
    }
}

#[test]
fn public_boundary_keeps_fractional_and_iso_compatibility() {
    // These spellings are not accepted by date arithmetic today. Keep the
    // public typed-error behavior rather than silently broadening the grammar.
    for duration in ["1.5d", "P1D", "PT24H"] {
        let expression = format!(r#""2026-01-01" + "{duration}""#);
        let error = eval(&expression).unwrap_err();
        assert_eq!(error.code, "type_error", "{duration}");
        assert_eq!(error.message, "Cannot add date and non-duration string");
    }
}

#[test]
fn public_overflow_diagnostics_are_stable_and_never_misstate_subtraction() {
    for (expression, duration) in [
        (
            r#""2026-07-22" + "9223372036854775807d""#,
            "9223372036854775807d",
        ),
        (
            r#""2026-07-22" - "9223372036854775807d""#,
            "9223372036854775807d",
        ),
        (
            r#""2026-07-22" - "-9223372036854775808d""#,
            "-9223372036854775808d",
        ),
        (
            r#""2026-07-22" + "9223372036854775807y""#,
            "9223372036854775807y",
        ),
    ] {
        let error = std::panic::catch_unwind(|| eval(expression))
            .expect("date arithmetic must not panic")
            .unwrap_err();
        assert_eq!(error.code, "type_error");
        assert_eq!(
            error.message,
            format!(
                "Date arithmetic overflow: date '2026-07-22' with duration '{duration}' is out of range"
            )
        );
        if expression.contains(" - ") {
            assert!(!error.message.contains(" + "), "{}", error.message);
        }
    }
}

#[test]
fn chrono_edges_do_not_panic_or_accidentally_expand_supported_date_syntax() {
    // Chrono's extrema require expanded years, which this public date grammar
    // does not recognize. They must remain ordinary string arithmetic rather
    // than reaching a panicking chrono operation.
    for expression in [
        r#""+262142-12-31" + "1d""#,
        r#""-262143-01-01" - "1d""#,
        r#""+262142-12-31T23:59:59" + "1d""#,
    ] {
        let result = std::panic::catch_unwind(|| eval(expression));
        assert!(result.is_ok(), "panicked: {expression}");
        let _ = result.unwrap();
    }
}
