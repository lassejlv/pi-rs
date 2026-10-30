use pi_telemetry::{
    SpanError, SpanStatus, StoredEvent, StoredLink, StoredSpan, attributes, render_trace,
};

fn span(id: &str) -> StoredSpan {
    StoredSpan {
        id: id.into(),
        trace_id: "trace".into(),
        parent_id: None,
        name: id.to_string(),
        started_ms: 0.0,
        ended_ms: 10.0,
        attributes: Default::default(),
        events: vec![],
        links: vec![],
        status: SpanStatus::Ok,
    }
}

fn with(mut base: StoredSpan, update: impl FnOnce(&mut StoredSpan)) -> StoredSpan {
    update(&mut base);
    base
}

fn link(trace: &str, span: &str) -> StoredLink {
    StoredLink {
        trace_id: trace.into(),
        span_id: span.into(),
        attributes: None,
    }
}

#[test]
fn renders_an_unordered_trace_with_attributes_durations_errors_events_and_links() {
    let root = with(span("root"), |s| {
        s.started_ms = 1000.0;
        s.ended_ms = 1020.0;
        s.attributes = attributes! {
            "request" => "a b",
            "attempt" => 2,
            "cached" => false,
            "tags" => vec!["a", "b"],
            "sizes" => vec![1.0, 2.0],
            "flags" => vec![true, false],
        };
        s.events = vec![
            StoredEvent {
                name: "checkpoint".into(),
                time_ms: 2.0,
                attributes: attributes! { "phase" => "tools" },
            },
            StoredEvent {
                name: "tick".into(),
                time_ms: 3.5,
                attributes: attributes! {},
            },
        ];
        s.links = vec![
            StoredLink {
                trace_id: "remote-trace".into(),
                span_id: "remote".into(),
                attributes: Some(attributes! { "reason" => "steer" }),
            },
            link("other-trace", "other"),
        ];
    });
    let first = with(span("first"), |s| {
        s.parent_id = Some("root".into());
        s.started_ms = 1005.0;
        s.ended_ms = 1009.5;
        s.status = SpanStatus::error(SpanError::new("Error", "failed call"));
    });
    let nested = with(span("nested"), |s| {
        s.parent_id = Some("first".into());
        s.started_ms = 1006.0;
        s.ended_ms = 1007.0;
    });
    let second = with(span("second"), |s| {
        s.parent_id = Some("root".into());
        s.started_ms = 1010.0;
        s.ended_ms = 1012.0;
        s.status = SpanStatus::Error(None);
    });
    let spans = vec![second, nested, first, root];
    let before = spans.clone();
    assert_eq!(
        render_trace(&spans),
        [
            r#"root request="a b" attempt=2 cached=false tags=["a","b"] sizes=[1,2] flags=[true,false] (20ms)"#,
            r#"  · checkpoint phase="tools" (+2ms)"#,
            "  · tick (+3.5ms)",
            r#"  → link trace remote-trace span remote reason="steer""#,
            "  → link trace other-trace span other",
            "  first (4.5ms) error Error: failed call",
            "    nested (1ms)",
            "  second (2ms) error",
        ]
        .join("\n")
    );
    assert_eq!(spans, before);
}

#[test]
fn treats_missing_parents_as_roots_and_orders_roots_and_children_by_start_time() {
    let spans = vec![
        with(span("later"), |s| {
            s.parent_id = Some("external".into());
            s.started_ms = 20.0;
            s.ended_ms = 30.0;
        }),
        with(span("child"), |s| {
            s.parent_id = Some("later".into());
            s.started_ms = 25.0;
            s.ended_ms = 30.0;
        }),
        with(span("earlier"), |s| {
            s.started_ms = 10.0;
            s.ended_ms = 15.0;
            s.links = vec![link("trace", "later")];
        }),
    ];
    assert_eq!(
        render_trace(&spans),
        [
            "earlier (5ms)",
            "  → link trace trace span later",
            "later (10ms)",
            "  child (5ms)"
        ]
        .join("\n")
    );
}

#[test]
fn prints_cross_trace_links_without_following_them_including_cycles() {
    let spans = vec![
        with(span("server:1"), |s| {
            s.name = "server".into();
            s.trace_id = "server-trace".into();
            s.links = vec![link("worker-trace", "worker:1")];
        }),
        with(span("worker:1"), |s| {
            s.name = "worker".into();
            s.trace_id = "worker-trace".into();
            s.links = vec![link("server-trace", "server:1")];
        }),
    ];
    assert_eq!(
        render_trace(&spans),
        [
            "server (10ms)",
            "  → link trace worker-trace span worker:1",
            "worker (10ms)",
            "  → link trace server-trace span server:1",
        ]
        .join("\n")
    );
}

#[test]
fn deduplicates_spans_merged_from_stores_by_global_id() {
    let root = with(span("server:1"), |s| s.name = "root".into());
    let child = with(span("worker:1"), |s| {
        s.name = "child".into();
        s.parent_id = Some("server:1".into());
        s.events = vec![StoredEvent {
            name: "done".into(),
            time_ms: 3.0,
            attributes: attributes! {},
        }];
    });
    let grandchild = with(span("worker:2"), |s| {
        s.name = "grandchild".into();
        s.parent_id = Some("worker:1".into());
    });
    assert_eq!(
        render_trace(&[child.clone(), root.clone(), grandchild, root, child]),
        "root (10ms)\n  child (10ms)\n    · done (+3ms)\n    grandchild (10ms)"
    );
}

#[test]
fn preserves_input_order_for_equal_start_times_without_mutating_the_input() {
    let spans = vec![
        span("root"),
        with(span("z"), |s| s.parent_id = Some("root".into())),
        with(span("a"), |s| s.parent_id = Some("root".into())),
        span("other"),
    ];
    assert_eq!(
        render_trace(&spans),
        "root (10ms)\n  z (10ms)\n  a (10ms)\nother (10ms)"
    );
}

#[test]
fn escapes_string_attribute_values_without_hiding_their_contents() {
    let spans = vec![with(span("root"), |s| {
        s.attributes = attributes! { "text" => "line one\n\"line two\"" }
    })];
    assert_eq!(
        render_trace(&spans),
        r#"root text="line one\n\"line two\"" (10ms)"#
    );
}

#[test]
fn renders_deep_trees_without_recursive_calls() {
    let spans: Vec<StoredSpan> = (0..2000)
        .map(|index| {
            with(span(&index.to_string()), |s| {
                if index > 0 {
                    s.parent_id = Some((index - 1).to_string().into());
                }
            })
        })
        .collect();
    let rendered = render_trace(&spans);
    assert_eq!(rendered.split('\n').count(), spans.len());
    assert!(rendered.ends_with(&format!("{}1999 (10ms)", "  ".repeat(1999))));
}

#[test]
fn renders_an_empty_input_as_an_empty_string() {
    assert_eq!(render_trace(&[]), "");
}
