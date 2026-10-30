//! Fixed tool instructions and a budgeted catalog of script-callable tools.
use super::{models::MODELS_DESCRIPTION, vm::identifier};
use crate::{
    harness::{
        declaration::ToolDeclaration,
        tool::{ExecutableTool, join},
    },
    *,
};
use indexmap::{IndexMap, IndexSet};
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;

pub const DEFAULT_CODEMODE_INLINE_BUDGET: f64 = 3000.0;
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CodemodeNamespace {
    pub name: JsonString,
    pub prefix: JsonString,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<JsonString>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<JsonString>,
}
#[derive(Clone, Debug)]
pub struct ToolMetadata {
    pub declaration: ToolDeclaration,
    pub structured_output_schema: Option<JsonValue>,
}
impl ToolMetadata {
    pub fn of(tool: &dyn ExecutableTool) -> Self {
        Self {
            declaration: tool.declaration(),
            structured_output_schema: tool.structured_output_schema(),
        }
    }
}
pub fn codemode_description(models: bool) -> JsonString {
    let mut parts = vec![INTRO.into(), GLOBALS.into()];
    if models {
        parts.push(MODELS_DESCRIPTION.into());
    }
    parts.push(FOOTER.into());
    join(parts, "\n\n")
}
pub fn namespace_of<'a>(
    name: &JsonString,
    namespaces: &'a [CodemodeNamespace],
) -> Option<&'a CodemodeNamespace> {
    let mut found: Option<&CodemodeNamespace> = None;
    for namespace in namespaces {
        if name.units().starts_with(namespace.prefix.units())
            && found.is_none_or(|prior| namespace.prefix.units().len() > prior.prefix.units().len())
        {
            found = Some(namespace);
        }
    }
    found
}
fn trimmed(value: &JsonString) -> JsonString {
    let space = |u: &u16| matches!(*u,0x09..=0x0d|0x20|0xa0|0x1680|0x2000..=0x200a|0x2028|0x2029|0x202f|0x205f|0x3000|0xfeff);
    let begin = value
        .units()
        .iter()
        .position(|u| !space(u))
        .unwrap_or(value.units().len());
    let end = value
        .units()
        .iter()
        .rposition(|u| !space(u))
        .map_or(begin, |i| i + 1);
    JsonString::from_utf16(value.units()[begin..end].to_vec())
}
pub fn sample_of(tool: &ToolMetadata) -> Result<JsonString> {
    let declaration = pi_codemode::declarations::JsonToolDeclaration {
        name: tool.declaration.name.clone(),
        description: Some(tool.declaration.description.clone()),
        input_schema: Some(tool.declaration.parameters.clone()),
        output_schema: Some(
            tool.structured_output_schema
                .as_ref()
                .filter(|schema| !schema.is_null())
                .cloned()
                .unwrap_or_else(|| wire::encode(&serde_json::json!({"type":"string"}))),
        ),
        ..Default::default()
    };
    Ok(pi_codemode::declarations::render_json_tool_sample(
        &declaration,
        None,
    ))
}

fn tool_section(tool: &ToolMetadata) -> Result<JsonString> {
    let name = &tool.declaration.name;
    let id = identifier(name);
    Ok(join(
        [
            "### `".into(),
            id.clone(),
            if id == *name {
                "`".into()
            } else {
                join(["` (`".into(), name.clone(), "`)".into()], "")
            },
            "\n".into(),
            trimmed(&sample_of(tool)?),
        ],
        "",
    ))
}
struct Entry {
    name: JsonString,
    section: JsonString,
    cost: f64,
}
struct Group<'a> {
    namespace: Option<&'a CodemodeNamespace>,
    entries: Vec<Entry>,
}
pub fn codemode_section(
    tools: &[ToolMetadata],
    namespaces: &[CodemodeNamespace],
    budget: f64,
) -> Result<Option<JsonString>> {
    if tools.is_empty() {
        return Ok(None);
    }
    let mut groups = IndexMap::<Option<JsonString>, Group>::new();
    groups.insert(
        None,
        Group {
            namespace: None,
            entries: vec![],
        },
    );
    for tool in tools {
        let namespace = namespace_of(&tool.declaration.name, namespaces);
        let key = namespace.map(|n| n.name.clone());
        let group = groups.entry(key).or_insert_with(|| Group {
            namespace,
            entries: vec![],
        });
        let section = tool_section(tool)?;
        group.entries.push(Entry {
            name: tool.declaration.name.clone(),
            cost: (section.units().len() as f64 / 4.0).ceil(),
            section,
        });
    }
    static COLLATOR: LazyLock<icu_collator::CollatorBorrowed<'static>> = LazyLock::new(|| {
        icu_collator::Collator::try_new(Default::default(), Default::default())
            .expect("compiled collation data")
    });
    let mut groups: Vec<_> = groups.into_values().collect();
    groups.sort_by(|a, b| match (a.namespace, b.namespace) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => std::cmp::Ordering::Less,
        (Some(_), None) => std::cmp::Ordering::Greater,
        (Some(a), Some(b)) => COLLATOR.compare_utf16(a.name.units(), b.name.units()),
    });
    let mut queues: Vec<_> = groups
        .iter()
        .map(|group| {
            let mut entries: Vec<_> = group.entries.iter().collect();
            entries.sort_by(|a, b| a.cost.total_cmp(&b.cost));
            std::collections::VecDeque::from(entries)
        })
        .filter(|queue| !queue.is_empty())
        .collect();
    let mut shown = IndexSet::new();
    let mut remaining = budget;
    while !queues.is_empty() {
        queues.retain_mut(|queue| {
            let next = queue.front().unwrap();
            if next.cost > remaining {
                return false;
            }
            remaining -= next.cost;
            shown.insert(next.name.clone());
            queue.pop_front();
            !queue.is_empty()
        });
    }
    let mut sections = vec!["Tools scripts can call (`await tools.<name>(args)`):".into()];
    for group in groups {
        if group.entries.is_empty() {
            continue;
        }
        let visible: Vec<_> = group
            .entries
            .iter()
            .filter(|entry| shown.contains(&entry.name))
            .collect();
        if let Some(namespace) = group.namespace {
            let listing = if visible.len() == group.entries.len() {
                ""
            } else if visible.is_empty() {
                " (tools not listed)"
            } else {
                " (some tools not listed)"
            };
            let description = namespace
                .description
                .as_ref()
                .map(trimmed)
                .unwrap_or_default();
            sections.push(join(
                [
                    "## ".into(),
                    namespace.name.clone(),
                    listing.into(),
                    if description.units().is_empty() {
                        JsonString::default()
                    } else {
                        join(["\n".into(), description], "")
                    },
                ],
                "",
            ));
        } else if visible.len() < group.entries.len() {
            sections.push("Some tools are not listed: find them with `searchTools()`.".into());
        }
        sections.extend(visible.iter().map(|entry| entry.section.clone()));
    }
    Ok(Some(join(sections, "\n\n")))
}

const INTRO: &str = r#"Run JavaScript that calls other tools. The input is raw JavaScript (not JSON, no code fence), run as an async function body in a QuickJS sandbox: top-level `await` and `return` work. No Node, file system, network, or timers.
- `await tools.<name>({ ...args })` resolves to a string, or an object if the tool's declaration says so, and rejects with an Error on failure. A tool whose declaration returns an object resolves on failure too when it returned that object; check its fields. Calls still running when the script ends are cancelled.
- The script survives a restart: it resumes where it was, without running finished calls again.
- Optional first line: `// @options: {"max_output_tokens": 10000, "timeout_ms": 60000}`"#;
const GLOBALS: &str = r#"Globals:
- `text(value)`, `image(dataUrlOrImageBlock)`, `console.log(...)`, and top-level `return` add output; `exit()` ends the script. With several text items, each starts with a `==> text N/M <==` line, and `console` lines follow the other output in one `<console_output>` block. `image()` also saves the image to a temp file and the result names its path.
- `store(key, value)` and `load(key)` keep JSON values across codemode calls.
- `ALL_TOOLS`, `await searchTools(query, { limit?, namespace? })`, `await describeTool(name)`, `await describeNamespace(name)`: find tools the `codemode` section does not list."#;
const FOOTER: &str = r#"The `codemode` section of the system prompt lists the tools scripts can call, by namespace, as far as space allows."#;
