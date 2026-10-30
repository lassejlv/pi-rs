use crate::{Context, Result, env::FileSystem};
use std::path::{Path, PathBuf};
use unicode_normalization::UnicodeNormalization;

pub fn normalize_tool_path(path: &str) -> String {
    let path = path
        .chars()
        .map(|ch| match ch {
            '\u{00a0}' | '\u{2000}'..='\u{200a}' | '\u{202f}' | '\u{205f}' | '\u{3000}' => ' ',
            other => other,
        })
        .collect::<String>();
    path.strip_prefix('@').unwrap_or(&path).to_owned()
}
pub async fn resolve_tool_path(
    env: &dyn FileSystem,
    path: &str,
    context: &Context,
) -> Result<PathBuf> {
    env.absolute_path(Path::new(&normalize_tool_path(path)), context)
        .await
        .map_err(super::file_error)
}
pub async fn resolve_read_tool_path(
    env: &dyn FileSystem,
    path: &str,
    context: &Context,
) -> Result<PathBuf> {
    let resolved = resolve_tool_path(env, path, context).await?;
    let text = resolved.to_string_lossy();
    let mut timestamp = String::new();
    for (index, ch) in text.char_indices() {
        let marker = text.get(index + 1..index + 4);
        timestamp.push(
            if ch == ' '
                && marker
                    .is_some_and(|m| m.eq_ignore_ascii_case("AM.") || m.eq_ignore_ascii_case("PM."))
            {
                '\u{202f}'
            } else {
                ch
            },
        );
    }
    let nfd = text.nfd().collect::<String>();
    let variants = [
        text.to_string(),
        timestamp,
        nfd.clone(),
        text.replace('\'', "\u{2019}"),
        nfd.replace('\'', "\u{2019}"),
    ];
    let mut visited = std::collections::HashSet::new();
    for variant in variants {
        if visited.insert(variant.clone())
            && env
                .exists(Path::new(&variant), context)
                .await
                .map_err(super::file_error)?
        {
            return Ok(variant.into());
        }
    }
    Ok(resolved)
}
