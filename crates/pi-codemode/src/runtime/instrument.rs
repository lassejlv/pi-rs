//! Loop instrumentation.
//!
//! The script's loops call a native tick function on every iteration: the tick counts the
//! execution's interrupt budget, which must count the same iterations on every run for
//! deterministic replays (QuickJS's interrupt handler runs every so many operations, which depends
//! on the engine's internals), and polls the host's interrupt flag (deadline, abort, a script that
//! already reported its result). It records why it stopped the script, so the VM can report it
//! even when an async function swallows the error. The call is inserted without adding line
//! breaks, so line numbers in stack traces are unchanged. Code produced at run time (`eval`,
//! `Function`) is not instrumented; QuickJS's interrupt handler bounds it with the loop iteration
//! limit and stops it when the host interrupts.

use oxc_allocator::Allocator;
use oxc_ast::ast::{DoWhileStatement, ForInStatement, ForOfStatement, ForStatement, Statement, WhileStatement};
use oxc_ast_visit::{Visit, walk};
use oxc_parser::Parser;
use oxc_span::{GetSpan, SourceType};

/// The global the instrumented loops call.
pub(crate) const TICK: &str = "__codemode$tick";

struct LoopFinder {
    inserts: Vec<(usize, &'static str)>,
}

impl LoopFinder {
    fn body(&mut self, body: &Statement<'_>) {
        match body {
            Statement::BlockStatement(block) => self.inserts.push((block.span.start as usize + 1, "__codemode$tick();")),
            other => {
                let span = other.span();
                self.inserts.push((span.start as usize, "{__codemode$tick();"));
                self.inserts.push((span.end as usize, "}"));
            }
        }
    }
}

impl<'a> Visit<'a> for LoopFinder {
    fn visit_while_statement(&mut self, it: &WhileStatement<'a>) {
        self.body(&it.body);
        walk::walk_while_statement(self, it);
    }

    fn visit_do_while_statement(&mut self, it: &DoWhileStatement<'a>) {
        self.body(&it.body);
        walk::walk_do_while_statement(self, it);
    }

    fn visit_for_statement(&mut self, it: &ForStatement<'a>) {
        self.body(&it.body);
        walk::walk_for_statement(self, it);
    }

    fn visit_for_in_statement(&mut self, it: &ForInStatement<'a>) {
        self.body(&it.body);
        walk::walk_for_in_statement(self, it);
    }

    fn visit_for_of_statement(&mut self, it: &ForOfStatement<'a>) {
        self.body(&it.body);
        walk::walk_for_of_statement(self, it);
    }
}

/// Ordered insertions for parsed loop bodies.
fn loop_inserts(source: &str) -> Option<Vec<(usize, &'static str)>> {
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, source, SourceType::script()).parse();
    if parsed.fatal_error || !parsed.diagnostics.is_empty() {
        return None;
    }
    let mut finder = LoopFinder { inserts: Vec::new() };
    finder.visit_program(&parsed.program);
    // Stable: a `}` closing one body and a `{` opening the next at the same offset keep their order.
    finder.inserts.sort_by_key(|(offset, _)| *offset);
    Some(finder.inserts)
}

pub(crate) fn instrument_loops(source: &str) -> Option<String> {
    let inserts = loop_inserts(source)?;
    let mut out = String::with_capacity(source.len() + inserts.len() * 20);
    let mut last = 0;
    for (offset, text) in inserts {
        if offset > source.len() || !source.is_char_boundary(offset) || offset < last {
            return None;
        }
        out.push_str(&source[last..offset]);
        out.push_str(text);
        last = offset;
    }
    out.push_str(&source[last..]);
    Some(out)
}

/// Parse an escaped UTF-8 view solely to locate loop boundaries. Apply those
/// insertions to the original UTF-16, preserving literal, comment and regex text.
pub(crate) fn instrument_loops_utf16(source: &[u16]) -> Option<Vec<u16>> {
    let mut parsed = String::new();
    let mut offsets = vec![Some(0)];
    let mut units = 0;
    for scalar in char::decode_utf16(source.iter().copied()) {
        let (text, consumed) = match scalar {
            Ok(ch) => (ch.to_string(), ch.len_utf16()),
            Err(error) => (format!("\\u{:04x}", error.unpaired_surrogate()), 1),
        };
        parsed.push_str(&text);
        offsets.resize(parsed.len(), None);
        units += consumed;
        offsets.push(Some(units));
    }
    let inserts = loop_inserts(&parsed)?;
    let mut result = Vec::with_capacity(source.len() + inserts.len() * 20);
    let mut last = 0;
    for (offset, text) in inserts {
        let offset = offsets.get(offset).copied().flatten()?;
        if offset < last || offset > source.len() {
            return None;
        }
        result.extend_from_slice(&source[last..offset]);
        result.extend(text.encode_utf16());
        last = offset;
    }
    result.extend_from_slice(&source[last..]);
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inserts_ticks_into_loop_bodies() {
        assert_eq!(
            instrument_loops("while (a) { b(); }\nfor (;;) x++;\ndo y(); while (z)").unwrap(),
            "while (a) {__codemode$tick(); b(); }\nfor (;;) {__codemode$tick();x++;}\ndo {__codemode$tick();y();} while (z)"
        );
        assert_eq!(
            instrument_loops("for (const a of b) for (const c in a) ;").unwrap(),
            "for (const a of b) {__codemode$tick();for (const c in a) {__codemode$tick();;}}"
        );
        assert!(instrument_loops("const = ;").is_none());
        assert_eq!(TICK, "__codemode$tick");
    }

    #[test]
    fn utf16_instrumentation_preserves_raw_units_and_maps_multibyte_offsets() {
        let prefix: Vec<u16> = "// \u{10000} é\nconst x = '".encode_utf16().chain([0xd800]).collect();
        let mut source = prefix.clone();
        source.extend("'; while(x) { f(); }".encode_utf16());
        let mut expected = prefix;
        expected.extend("'; while(x) {__codemode$tick(); f(); }".encode_utf16());
        assert_eq!(instrument_loops_utf16(&source).unwrap(), expected);
    }
}
