//! `pi_embed::include_str!`: `include_str!` that embeds the file deflate-compressed.

use std::io::Write as _;
use std::path::PathBuf;

use proc_macro::{Delimiter, Group, Literal, Span, TokenStream, TokenTree};

/// `include_str!(path)` as a `pi_embed::Embedded`: the file is read and compressed at compile
/// time and inflated once on first use. A relative path is resolved against the invoking file, as
/// with `include_str!`.
#[proc_macro]
pub fn include_str(input: TokenStream) -> TokenStream {
    match expand(input) {
        Ok(tokens) => tokens,
        Err((span, message)) => compile_error(span, &message),
    }
}

fn expand(input: TokenStream) -> Result<TokenStream, (Span, String)> {
    let mut tokens = input.into_iter();
    let literal = match (tokens.next(), tokens.next()) {
        (Some(TokenTree::Literal(literal)), None) => literal,
        // `include_str!` of a path built by another macro arrives wrapped in an invisible group.
        (Some(TokenTree::Group(group)), None) if group.delimiter() == Delimiter::None => {
            return expand(group.stream());
        }
        (Some(other), _) => return Err((other.span(), "expected a string literal path".into())),
        (None, _) => return Err((Span::call_site(), "expected a string literal path".into())),
    };
    let span = literal.span();
    let rel = unquote(&literal.to_string()).ok_or((span, "expected a string literal path".into()))?;

    let mut path = PathBuf::from(&rel);
    if path.is_relative() {
        let file = Span::call_site()
            .local_file()
            .ok_or((span, format!("cannot resolve relative path {rel:?}: invoking file has no local path")))?;
        path = file.parent().unwrap_or(&file).join(path);
    }
    // `local_file` is relative to rustc's working directory; `include_bytes!` below would resolve
    // a relative path against the invoking file instead.
    if path.is_relative() {
        let cwd = std::env::current_dir().map_err(|e| (span, format!("cannot resolve {rel:?}: {e}")))?;
        path = cwd.join(path);
    }
    let text = std::fs::read_to_string(&path).map_err(|e| (span, format!("couldn't read {}: {e}", path.display())))?;

    let mut encoder = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::best());
    encoder.write_all(text.as_bytes()).expect("compress into memory");
    let compressed = encoder.finish().expect("compress into memory");

    // `include_bytes!` in an unused constant records the file as a build input (so editing it
    // rebuilds the crate) without embedding its contents.
    let path_literal = Literal::string(path.to_str().ok_or((span, "path is not UTF-8".into()))?);
    let rest = format!(
        "; const _: &[u8] = ::core::include_bytes!({path_literal}); \
         static CELL: ::std::sync::OnceLock<::std::boxed::Box<str>> = ::std::sync::OnceLock::new(); \
         ::pi_embed::Embedded::__new(DATA, {len}, &CELL)",
        len = text.len(),
    );
    let mut block: TokenStream = "const DATA: &[u8] =".parse().expect("valid tokens");
    block.extend([TokenTree::Literal(Literal::byte_string(&compressed))]);
    block.extend(rest.parse::<TokenStream>().expect("valid tokens"));
    Ok(TokenTree::Group(Group::new(Delimiter::Brace, block)).into())
}

/// The value of a plain or raw string literal token.
fn unquote(token: &str) -> Option<String> {
    if let Some(rest) = token.strip_prefix('r') {
        let hashes = rest.len() - rest.trim_start_matches('#').len();
        let inner = rest.get(hashes + 1..rest.len().checked_sub(hashes + 1)?)?;
        return Some(inner.to_string());
    }
    let inner = token.strip_prefix('"')?.strip_suffix('"')?;
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next()? {
            '\\' => out.push('\\'),
            '"' => out.push('"'),
            '\'' => out.push('\''),
            'n' => out.push('\n'),
            't' => out.push('\t'),
            'r' => out.push('\r'),
            '0' => out.push('\0'),
            _ => return None,
        }
    }
    Some(out)
}

fn compile_error(span: Span, message: &str) -> TokenStream {
    let tokens: TokenStream = format!("::core::compile_error!({})", Literal::string(message)).parse().expect("valid tokens");
    tokens
        .into_iter()
        .map(|mut tree| {
            tree.set_span(span);
            tree
        })
        .collect()
}
