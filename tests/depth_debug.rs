use mdbase::expressions::parser::Parser;
use mdbase::expressions::evaluator::{evaluate, EvalContext};

#[test]
fn test_depth_65() {
    let mut expr = String::from("value");
    for _ in 0..65 {
        expr = format!("if(true, {}, 0)", expr);
    }
    
    eprintln!("=== Parsing 65-level ({} chars) ===", expr.len());
    match Parser::parse(&expr) {
        Ok(parsed) => {
            eprintln!("Parse OK");
            let ctx = EvalContext::empty();
            match evaluate(&parsed, &ctx) {
                Ok(val) => eprintln!("Eval OK: {:?}", val),
                Err(e) => eprintln!("Eval ERROR: code={}, message={}", e.code, e.message),
            }
        }
        Err(e) => eprintln!("Parse ERROR: {}", e),
    }
}

#[test]
fn test_depth_64() {
    let mut expr = String::from("value");
    for _ in 0..64 {
        expr = format!("if(true, {}, 0)", expr);
    }
    
    eprintln!("=== Parsing 64-level ({} chars) ===", expr.len());
    match Parser::parse(&expr) {
        Ok(parsed) => {
            eprintln!("Parse OK");
            let ctx = EvalContext::empty();
            match evaluate(&parsed, &ctx) {
                Ok(val) => eprintln!("Eval OK: {:?}", val),
                Err(e) => eprintln!("Eval ERROR: code={}, message={}", e.code, e.message),
            }
        }
        Err(e) => eprintln!("Parse ERROR: {}", e),
    }
}
