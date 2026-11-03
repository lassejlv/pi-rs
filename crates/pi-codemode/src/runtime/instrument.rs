//! Loop instrumentation.
//!
//! The script's loops call a native tick on every iteration to count the interrupt budget.
//! djs also charges interpreter fuel, including promise jobs and generated code. Fatal stops
//! abort the interpreter, so async functions cannot swallow cancellation or exhaustion.
//! Insertions add no line breaks; columns on instrumented lines may shift.


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

/// `source` with a tick call at the start of every loop body, or `None` when it does not parse
/// (the engine then reports the syntax error).
pub(crate) fn instrument_loops(source: &str) -> Option<String> {
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, source, SourceType::script()).parse();
    if parsed.fatal_error || !parsed.diagnostics.is_empty() {
        return None;
    }
    let mut finder = LoopFinder { inserts: Vec::new() };
    finder.visit_program(&parsed.program);
    if finder.inserts.is_empty() {
        return Some(source.to_string());
    }
    // Stable: a `}` closing one body and a `{` opening the next at the same offset keep their order.
    finder.inserts.sort_by_key(|(offset, _)| *offset);
    let mut out = String::with_capacity(source.len() + finder.inserts.len() * 20);
    let mut last = 0;
    for (offset, text) in finder.inserts {
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
}
