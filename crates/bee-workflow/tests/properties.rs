//! Property tests: no input makes the engine panic, and matrix sizes follow the product rule.

use bee_workflow::expr::{Condition, MapContext, Template, eval_str, parse};
use bee_workflow::glob::Pattern;
use bee_workflow::matrix::{MAX_MATRIX_JOBS, MatrixError, expand, product_size};
use bee_workflow::parse::parse_workflow;
use bee_workflow::value::{Object, Value, parse_json};
use proptest::prelude::*;

fn ctx() -> MapContext<'static> {
    MapContext::default()
        .with(
            "github",
            parse_json(r#"{"event_name":"push","ref":"refs/heads/main","event":{"labels":[{"name":"a"},{"name":"b"}],"n":3}}"#).unwrap(),
        )
        .with("matrix", parse_json(r#"{"os":"linux","v":[1,2,{"x":null}]}"#).unwrap())
}

/// Fragments that combine into mostly-valid and often-invalid expressions.
fn fragment() -> impl Strategy<Value = &'static str> {
    prop::sample::select(vec![
        "github",
        "matrix",
        ".",
        "*",
        "event",
        "labels",
        "name",
        "[",
        "]",
        "(",
        ")",
        "0",
        "1.5",
        "-2",
        "0xff",
        "'s'",
        "'it''s'",
        "null",
        "true",
        "false",
        "NaN",
        "Infinity",
        "!",
        "==",
        "!=",
        "<",
        "<=",
        ">",
        ">=",
        "&&",
        "||",
        ",",
        "contains(",
        "startsWith(",
        "endsWith(",
        "format('{0}{1}'",
        "join(",
        "toJSON(",
        "fromJSON(",
        "'[1,{}]'",
        "success()",
        "always()",
        "failure()",
        "cancelled()",
        "hashFiles('x')",
        "os",
        "v",
        "x",
        " ",
    ])
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2000))]

    #[test]
    fn expression_evaluation_never_panics(parts in prop::collection::vec(fragment(), 0..24)) {
        let src = parts.concat();
        let _ = eval_str(&src, &ctx());
        let _ = Condition::parse(&src).map(|c| c.evaluate(&ctx()));
        let _ = Template::parse(&format!("a ${{{{ {src} }}}} b")).map(|t| t.evaluate(&ctx()));
    }

    #[test]
    fn arbitrary_text_never_panics_the_parsers(src in "\\PC{0,80}") {
        let _ = parse(&src);
        let _ = eval_str(&src, &ctx());
        let _ = Template::parse(&src);
        let _ = parse_json(&src);
        let _ = Pattern::new(&src).matches("refs/heads/main");
        let _ = Pattern::new("**/*.rs").matches(&src);
    }

    #[test]
    fn arbitrary_yaml_never_panics(lines in prop::collection::vec(
        prop::sample::select(vec![
            "on: push", "on:", "  push:", "    branches: [main]", "jobs:", "  a:", "    runs-on: x",
            "    steps:", "      - run: echo ${{ github.ref }}", "      - uses: a/b@v1", "        with:",
            "          k: v", "    strategy:", "      matrix:", "        os: [a, b]", "        include:",
            "          - os: c", "    needs: a", "    if: always()", "  - x", "- y", "k: |", "  text",
            "'q': \"w\"", "a: &x 1", "b: *x", "c: {d: [1, 2]}", "#c", "", "   ", "\t", "key: [", "}",
        ]),
        0..30,
    )) {
        let src = lines.join("\n");
        let _ = parse_workflow(".github/workflows/p.yml", &src);
    }

    #[test]
    fn matrix_size_is_the_product_of_vector_lengths(lens in prop::collection::vec(1usize..6, 1..5)) {
        let mut m = Object::new();
        for (i, n) in lens.iter().enumerate() {
            m.insert(format!("k{i}"), Value::array((0..*n).map(|j| Value::Number(j as f64))));
        }
        let product: usize = lens.iter().product();
        prop_assert_eq!(product_size(&m), product as u64);
        let v = Value::from(m);
        match expand(&v) {
            Ok(combos) => {
                prop_assert!(product <= MAX_MATRIX_JOBS);
                prop_assert_eq!(combos.len(), product);
                // Every combination is distinct and has every key.
                for (i, a) in combos.iter().enumerate() {
                    prop_assert_eq!(a.len(), lens.len());
                    for b in &combos[i + 1..] {
                        prop_assert!(a != b);
                    }
                }
            }
            Err(MatrixError::TooLarge(n)) => {
                prop_assert!(product > MAX_MATRIX_JOBS);
                prop_assert_eq!(n, product as u64);
            }
            Err(e) => prop_assert!(false, "unexpected error {e}"),
        }
    }

    #[test]
    fn excludes_remove_exactly_the_matching_combinations(a in 1usize..5, b in 1usize..5, drop in 0usize..5) {
        let mut m = Object::new();
        m.insert("a", Value::array((0..a).map(|j| Value::Number(j as f64))));
        m.insert("b", Value::array((0..b).map(|j| Value::Number(j as f64))));
        // Exclude every combination with a == drop (if it exists): removes b combinations.
        m.insert("exclude", Value::array([Value::object([("a", Value::Number(drop as f64))])]));
        let combos = expand(&Value::from(m)).unwrap();
        let expected = if drop < a { (a - 1) * b } else { a * b };
        prop_assert_eq!(combos.len(), expected);
    }

    #[test]
    fn includes_never_change_original_values(a in 1usize..4, extra in 0usize..4) {
        let mut m = Object::new();
        m.insert("a", Value::array((0..a).map(|j| Value::Number(j as f64))));
        m.insert("include", Value::array((0..extra).map(|j| Value::object([("a", Value::Number(j as f64)), ("tag", Value::from("t"))]))));
        let combos = expand(&Value::from(m)).unwrap();
        // Includes matching an original value extend it; others add one combination each.
        let added = (0..extra).filter(|j| *j >= a).count();
        prop_assert_eq!(combos.len(), a + added);
        for (i, c) in combos.iter().take(a).enumerate() {
            prop_assert_eq!(c.get("a"), Some(&Value::Number(i as f64)));
        }
    }
}
