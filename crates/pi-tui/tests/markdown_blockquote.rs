mod common;

use common::chalk;
use pi_tui::components::markdown::{DefaultTextStyle, Markdown, MarkdownTheme};
use pi_tui::tui::Component;
use pi_tui::utils::visible_width;

#[test]
fn blockquotes_without_room_for_a_border_preserve_content() {
    for width in [0, 1, 2] {
        let markdown = Markdown::new(">>>>>x", 0, 0, MarkdownTheme::plain(), None, None);
        let lines = markdown.render(width);
        assert_eq!(lines.len(), 1, "width {width}");
        assert_eq!(lines[0].trim_end(), "x", "width {width}");
    }
}

#[test]
fn deeply_nested_blockquotes_have_bounded_output() {
    for depth in [40, 60] {
        for (width, padding) in [(1, 0), (2, 0), (3, 0), (4, 0), (80, 1)] {
            let markdown = Markdown::new(
                format!("{}x", ">".repeat(depth)),
                padding,
                0,
                MarkdownTheme::plain(),
                None,
                None,
            );
            let lines = markdown.render(width);
            assert_eq!(lines.len(), 1, "depth {depth}, width {width}");
            let content_width = width - 2 * padding;
            let borders = (content_width - 1) / 2;
            assert_eq!(
                lines[0].trim_end(),
                format!("{}{}x", " ".repeat(padding), "│ ".repeat(borders))
            );
            assert_eq!(visible_width(&lines[0]), width);
        }
    }
}

#[test]
fn ordinary_blockquotes_keep_styling_and_spacing() {
    let markdown = Markdown::new(
        "> Foo\nbar\n\nafter",
        0,
        0,
        chalk::markdown_theme(),
        Some(DefaultTextStyle {
            color: Some(chalk::style(&[("\x1b[35m", "\x1b[39m")])),
            ..Default::default()
        }),
        None,
    );
    let lines = markdown.render(20);
    assert_eq!(lines.len(), 4);
    for (line, text) in lines.iter().zip(["Foo", "bar"]) {
        assert!(line.starts_with("\x1b[2m│ \x1b[22m"));
        assert!(line.contains(text));
        assert!(line.contains("\x1b[3m"));
        assert!(!line.contains("\x1b[35m"));
        assert_eq!(visible_width(line), 20);
    }
    assert_eq!(lines[2].trim_end(), "");
    assert!(lines[3].contains("\x1b[35m"));
}

#[test]
fn deeply_nested_styled_blockquotes_keep_content_without_growing() {
    let markdown = Markdown::new(
        format!("{}x", ">".repeat(60)),
        1,
        0,
        chalk::markdown_theme(),
        None,
        None,
    );
    let lines = markdown.render(80);
    assert_eq!(lines.len(), 1);
    assert_eq!(visible_width(&lines[0]), 80);
    assert!(lines[0].contains('x'));
    let shallower = Markdown::new(
        format!("{}x", ">".repeat(40)),
        1,
        0,
        chalk::markdown_theme(),
        None,
        None,
    )
    .render(80);
    assert_eq!(lines, shallower, "extra nesting must not grow ANSI output");
}

#[test]
fn borderless_blockquotes_keep_inherited_style_and_block_spacing() {
    for width in [1, 2] {
        let render = |source: &str| {
            Markdown::new(
                source,
                0,
                0,
                chalk::markdown_theme(),
                Some(DefaultTextStyle {
                    color: Some(chalk::style(&[chalk::BLUE])),
                    ..Default::default()
                }),
                None,
            )
            .render(width)
        };
        assert_eq!(render("> a\n>\n> b\n\nafter"), render("a\n\nb\n\nafter"));
    }
}
