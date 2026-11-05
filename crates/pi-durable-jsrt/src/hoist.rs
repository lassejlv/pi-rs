//! vitest's mock hoisting for test files run by the embedded runner.
//!
//! vitest moves every top-level `vi.mock(...)` and `vi.hoisted(...)` above the file's imports and
//! turns the other imports into dynamic imports, so mocks are registered before any module is
//! loaded. [`hoist_vitest_mocks`] does the same on the type-stripped source:
//!
//! ```text
//! import { __applyMocks as __piDurableApplyMocks } from "vitest";
//! <top-level vi.hoisted / vi.mock / vi.unmock statements, in order>
//! await __piDurableApplyMocks(import.meta.url);   // build and install every registered mock
//! const { "a": a, "default": b } = await import("x");   // each non-vitest import, in order
//! <the rest of the file; imports of "vitest" stay static>
//! ```
//!
//! The runner serves the result as the test file's source ([`pi_jsrt::JsRuntime::set_file_source`]),
//! so the file on disk is untouched. Bindings of converted imports are `const` snapshots (vitest
//! rewrites references to keep them live; no upstream test reassigns an import's exports).

use oxc_allocator::Allocator;
use oxc_ast::ast::{Expression, ImportDeclarationSpecifier, ImportOrExportKind, Statement};
use oxc_parser::Parser;
use oxc_span::SourceType;

fn is_vi_call(expr: &Expression<'_>, names: &[&str]) -> bool {
    let expr = match expr {
        Expression::AwaitExpression(a) => &a.argument,
        other => other,
    };
    let Expression::CallExpression(call) = expr else {
        return false;
    };
    let Expression::StaticMemberExpression(member) = &call.callee else {
        return false;
    };
    matches!(&member.object, Expression::Identifier(id) if id.name == "vi" || id.name == "vitest")
        && names.contains(&member.property.name.as_str())
}

const HOISTED: &[&str] = &["mock", "hoisted", "unmock"];

/// Hoist a test file's `vi.mock` / `vi.hoisted` calls (see the module docs). `Ok(None)` when the
/// file has none; `path` names the file in errors.
pub fn hoist_vitest_mocks(source: &str, path: &str) -> Result<Option<String>, String> {
    if !source.contains("vi.mock(")
        && !source.contains("vi.hoisted(")
        && !source.contains("vi.unmock(")
    {
        return Ok(None);
    }
    let js = pi_jsrt::transpile::strip_types(source).map_err(|e| format!("{path}: {e}"))?;
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, &js, SourceType::mjs()).parse();
    if parsed.fatal_error || parsed.diagnostics.has_errors() {
        let error = parsed
            .diagnostics
            .errors()
            .next()
            .map(|e| e.to_string())
            .unwrap_or_default();
        return Err(format!(
            "{path}: cannot parse the type-stripped source: {error}"
        ));
    }
    let mut hoisted = Vec::new();
    let mut imports = Vec::new();
    let mut removed = Vec::new();
    for statement in &parsed.program.body {
        match statement {
            Statement::ImportDeclaration(import)
                if import.import_kind != ImportOrExportKind::Type =>
            {
                let spec = import.source.value.display().to_string();
                if spec == "vitest" {
                    continue;
                }
                let quoted = serde_json::to_string(&spec).unwrap_or_default();
                let Some(specifiers) = &import.specifiers else {
                    imports.push(format!("await import({quoted});"));
                    removed.push(import.span);
                    continue;
                };
                let mut fields = Vec::new();
                for specifier in specifiers {
                    match specifier {
                        ImportDeclarationSpecifier::ImportSpecifier(s)
                            if s.import_kind != ImportOrExportKind::Type =>
                        {
                            let name = serde_json::to_string(&s.imported.name().to_string())
                                .unwrap_or_default();
                            fields.push(format!("{name}: {}", s.local.name));
                        }
                        ImportDeclarationSpecifier::ImportSpecifier(_) => {}
                        ImportDeclarationSpecifier::ImportDefaultSpecifier(s) => {
                            fields.push(format!("\"default\": {}", s.local.name))
                        }
                        ImportDeclarationSpecifier::ImportNamespaceSpecifier(s) => {
                            imports
                                .push(format!("const {} = await import({quoted});", s.local.name));
                        }
                    }
                }
                if !fields.is_empty() {
                    imports.push(format!(
                        "const {{ {} }} = await import({quoted});",
                        fields.join(", ")
                    ));
                }
                removed.push(import.span);
            }
            Statement::ExpressionStatement(s) if is_vi_call(&s.expression, HOISTED) => {
                hoisted.push(js[s.span.start as usize..s.span.end as usize].to_string());
                removed.push(s.span);
            }
            Statement::VariableDeclaration(d)
                if d.declarations
                    .iter()
                    .any(|v| v.init.as_ref().is_some_and(|e| is_vi_call(e, &["hoisted"]))) =>
            {
                hoisted.push(js[d.span.start as usize..d.span.end as usize].to_string());
                removed.push(d.span);
            }
            _ => {}
        }
    }
    // Blank moved statements, keeping their line breaks so later lines keep their numbers
    // relative to each other.
    let mut body = js.clone().into_bytes();
    for span in &removed {
        for b in &mut body[span.start as usize..span.end as usize] {
            if *b != b'\n' {
                *b = b' ';
            }
        }
    }
    let body = String::from_utf8(body).map_err(|e| format!("{path}: {e}"))?;
    let mut out =
        String::from("import { __applyMocks as __piDurableApplyMocks } from \"vitest\";\n");
    for h in hoisted {
        out.push_str(&h);
        out.push('\n');
    }
    out.push_str("await __piDurableApplyMocks(import.meta.url);\n");
    for i in imports {
        out.push_str(&i);
        out.push('\n');
    }
    out.push_str(&body);
    Ok(Some(out))
}
