use std::collections::HashMap;

use crate::{
    Action, Axis, DimArea, EvalError, Expr, Facts, Filter, Item, Macro, MacroOption, MonitorRef,
    Op, ResizeBlock, eval, parse,
};

struct Fixed(HashMap<Axis, f64>);

impl Facts for Fixed {
    fn dimension(&self, axis: Axis, _area: &DimArea) -> Result<f64, EvalError> {
        Ok(*self.0.get(&axis).unwrap_or(&0.0))
    }
}

fn monitor(width: f64, height: f64) -> Fixed {
    Fixed(HashMap::from([
        (Axis::Width, width),
        (Axis::Height, height),
    ]))
}

fn block(mac: &Macro) -> &ResizeBlock {
    mac.items
        .iter()
        .find_map(|item| match item {
            Item::Resize(block) => Some(block),
            _ => None,
        })
        .expect("a resize block")
}

/// Parse `source` and evaluate the first assignment of the first resize block.
fn value_of(source: &str) -> Expr {
    let mac = parse(&format!("resize w = {source}")).expect("expression must parse");
    let block = block(&mac);
    let Action::Assign { value, .. } = &block.body[0] else {
        panic!("expected an assignment");
    };
    value.clone()
}

#[test]
fn parses_the_official_four_column_example() {
    let mac = parse(
        "resize : proc[notepad]*\n\
         t = 300 * (($_index - 1) \\ 4)\n\
         l = 400 * (($_index - 1) % 4)\n\
         w = 400\n\
         h = 300",
    )
    .expect("official example must parse");
    assert_eq!(mac.items.len(), 1);
    let block = block(&mac);
    assert!(block.all, "the trailing * makes it a multi-window block");
    assert_eq!(
        block.filters,
        vec![Filter::Proc {
            text: "notepad".into(),
            desktop: false,
            index: None
        }]
    );
    assert_eq!(block.body.len(), 4);
    assert!(matches!(
        block.body[0],
        Action::Assign {
            axis: Axis::Top,
            ..
        }
    ));
    assert!(matches!(
        block.body[2],
        Action::Assign {
            axis: Axis::Width,
            ..
        }
    ));
    let Action::Assign { value, .. } = &block.body[0] else {
        unreachable!()
    };
    let Expr::Binary {
        op: Op::Mul, right, ..
    } = value
    else {
        panic!("expected a multiplication, got {value:?}");
    };
    assert!(matches!(**right, Expr::Binary { op: Op::IntDiv, .. }));
}

#[test]
fn parses_window_qualifiers() {
    let last = parse("resize : last t = 0").unwrap();
    assert_eq!(block(&last).filters, vec![Filter::Last]);
    let active = parse("resize : active t = 0").unwrap();
    assert_eq!(block(&active).filters, vec![Filter::Active]);

    let capped = parse("resize : cap[\"My App\"]:desktop:2* w = 100").unwrap();
    assert_eq!(
        block(&capped).filters,
        vec![Filter::Cap {
            text: "My App".into(),
            desktop: true,
            index: Some(2)
        }]
    );
    assert!(block(&capped).all);

    // A `:` followed by another filter joins filters instead of starting a suffix.
    let chained = parse("resize : proc[one]:cap[two]* w = 10").unwrap();
    assert_eq!(block(&chained).filters.len(), 2);
    assert!(block(&chained).all);
}

#[test]
fn parses_bare_resize_and_actions() {
    let bare = parse("resize").unwrap();
    assert!(block(&bare).filters.is_empty());
    assert!(block(&bare).body.is_empty());

    let actions = parse("resize : active restore minimize maximize:primary").unwrap();
    let body = &block(&actions).body;
    assert!(matches!(body[0], Action::Restore { .. }));
    assert!(matches!(body[1], Action::Minimize { .. }));
    assert!(matches!(
        body[2],
        Action::Maximize {
            monitor: Some(MonitorRef::Primary),
            ..
        }
    ));

    let current = parse("resize maximize:current").unwrap();
    assert!(matches!(
        block(&current).body[0],
        Action::Maximize {
            monitor: Some(MonitorRef::Current),
            ..
        }
    ));
    // `active` is the documented synonym of `current` in a monitor qualifier.
    let active = parse("resize maximize:active").unwrap();
    assert!(matches!(
        block(&active).body[0],
        Action::Maximize {
            monitor: Some(MonitorRef::Current),
            ..
        }
    ));
}

#[test]
fn parses_options() {
    let mac =
        parse("option compensate_frame_effects on\noption ignore_runtime_errors off").unwrap();
    let Item::Option {
        option, enabled, ..
    } = &mac.items[0]
    else {
        panic!("expected an option")
    };
    assert_eq!(*option, MacroOption::CompensateFrameEffects);
    assert!(*enabled);
    let Item::Option {
        option, enabled, ..
    } = &mac.items[1]
    else {
        panic!("expected an option")
    };
    assert_eq!(*option, MacroOption::IgnoreRuntimeErrors);
    assert!(!*enabled);
}

#[test]
fn parses_dimensions_and_monitor_qualifiers() {
    let scaled = parse("resize w = 0.7 * w:mon h = 10").unwrap();
    let Action::Assign { value, .. } = &block(&scaled).body[0] else {
        unreachable!()
    };
    let Expr::Binary { right, .. } = value else {
        panic!("expected a multiplication")
    };
    assert_eq!(
        **right,
        Expr::Dimension(crate::Dimension {
            axis: Axis::Width,
            area: DimArea::Mon(None)
        })
    );

    let area = parse("resize w = w:workarea:primary").unwrap();
    let Action::Assign {
        value: Expr::Dimension(dimension),
        ..
    } = &block(&area).body[0]
    else {
        panic!("expected a dimension")
    };
    assert_eq!(dimension.area, DimArea::WorkArea(Some(MonitorRef::Primary)));

    let client = parse("resize l = t:client:proc[x]").unwrap();
    let Action::Assign {
        value: Expr::Dimension(dimension),
        ..
    } = &block(&client).body[0]
    else {
        panic!("expected a dimension")
    };
    assert!(matches!(dimension.area, DimArea::Client(Some(_))));

    let readable = parse("resize h = r:win").unwrap();
    let Action::Assign {
        value: Expr::Dimension(dimension),
        ..
    } = &block(&readable).body[0]
    else {
        panic!("expected a dimension")
    };
    assert_eq!(dimension.axis, Axis::Right);
    assert_eq!(dimension.area, DimArea::Win(None));
}

#[test]
fn parses_variables_including_system_ones() {
    let mac =
        parse("$gap = 10\nresize : proc[x]* $_index = $_index l = $gap * ($_count - 1)").unwrap();
    assert!(matches!(&mac.items[0], Item::Assign { name, .. } if name == "$gap"));
    let body = &block(&mac).body;
    assert!(matches!(&body[0], Action::Variable { name, .. } if name == "$_index"));
    let Action::Assign { axis, value, .. } = &body[1] else {
        panic!("expected an assignment")
    };
    assert_eq!(*axis, Axis::Left);
    let Expr::Binary { right, .. } = value else {
        panic!("expected gap * ($_count - 1)")
    };
    assert!(matches!(**right, Expr::Binary { op: Op::Sub, .. }));
}

#[test]
fn rejects_tokens_outside_the_productions() {
    for source in [
        "#define W 100",
        "#include other.mac",
        "spacer 10",
        "resize : grid",
        "option compensate_frame_effects maybe",
        "option unknown_option on",
        "resize : proc[x] r = 10",
        "resize : proc[x] t 10",
        "resize : proc[x",
        "resize :",
        "resize : 4",
        "resize w = 1 +",
        "resize w = 1.2.3",
        "resize : cap[x] = 1",
    ] {
        let error = parse(source).expect_err(&format!("{source:?} must be rejected"));
        assert!(
            error.span.line >= 1 && error.span.column >= 1,
            "{source:?}: {error}"
        );
    }
}

#[test]
fn errors_carry_a_position() {
    let error = parse("resize w = 10\nresize : proc[x]\n  bogus").unwrap_err();
    assert_eq!((error.span.line, error.span.column), (3, 3), "{error}");
    assert!(
        error.message.contains("Unexpected token in macro"),
        "{error}"
    );

    let error = parse("resize : proc[x] t = 1\n#oops").unwrap_err();
    assert_eq!((error.span.line, error.span.column), (2, 1), "{error}");

    // A dimension letter that cannot be assigned is named in the error.
    let error = parse("resize r = 10").unwrap_err();
    assert!(error.message.contains("cannot be assigned"), "{error}");
}

#[test]
fn evaluates_the_official_index_arithmetic() {
    let mac = parse(
        "resize : proc[notepad]*\n\
         t = 300 * (($_index - 1) \\ 4)\n\
         l = 400 * (($_index - 1) % 4)",
    )
    .unwrap();
    let Action::Assign { value: top, .. } = &block(&mac).body[0] else {
        unreachable!()
    };
    let Action::Assign { value: left, .. } = &block(&mac).body[1] else {
        unreachable!()
    };
    let facts = monitor(1920.0, 1080.0);
    let mut tops = Vec::new();
    let mut lefts = Vec::new();
    // Eight windows fill two rows of four: the fifth one starts the second row.
    for index in 1..=8 {
        let variables = HashMap::from([("$_index".to_string(), index as f64)]);
        tops.push(eval(top, &variables, &facts).unwrap());
        lefts.push(eval(left, &variables, &facts).unwrap());
    }
    assert_eq!(tops, vec![0.0, 0.0, 0.0, 0.0, 300.0, 300.0, 300.0, 300.0]);
    assert_eq!(
        lefts,
        vec![0.0, 400.0, 800.0, 1200.0, 0.0, 400.0, 800.0, 1200.0]
    );
}

#[test]
fn evaluates_operators_and_dimensions() {
    let facts = monitor(1000.0, 800.0);
    let variables = HashMap::new();
    assert_eq!(
        eval(&value_of("0.7 * w:mon"), &variables, &facts).unwrap(),
        700.0
    );
    assert_eq!(
        eval(&value_of("2 + 3 * 4"), &variables, &facts).unwrap(),
        14.0
    );
    assert_eq!(
        eval(&value_of("(2 + 3) * 4"), &variables, &facts).unwrap(),
        20.0
    );
    assert_eq!(
        eval(&value_of("-3 \\ 2"), &variables, &facts).unwrap(),
        -2.0
    );
    assert_eq!(eval(&value_of("-7 % 4"), &variables, &facts).unwrap(), -3.0);
    assert_eq!(eval(&value_of("+4"), &variables, &facts).unwrap(), 4.0);
    assert_eq!(
        eval(&value_of("h:mon / 2"), &variables, &facts).unwrap(),
        400.0
    );
}

#[test]
fn evaluation_errors_are_explicit() {
    let facts = monitor(1000.0, 800.0);
    let undefined = eval(&value_of("$missing + 1"), &HashMap::new(), &facts).unwrap_err();
    assert!(undefined.message.contains("$missing"), "{undefined}");
    let zero = eval(&value_of("10 / 0"), &HashMap::new(), &facts).unwrap_err();
    assert!(zero.message.contains("division by zero"), "{zero}");
    let zero = eval(&value_of("10 \\ 0"), &HashMap::new(), &facts).unwrap_err();
    assert!(zero.message.contains("division by zero"), "{zero}");
}
