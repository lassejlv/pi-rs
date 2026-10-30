//! Text encoding, base64, URL parsing and grapheme segmentation ops.

use base64::Engine as _;
use crate::js::{Context, JsArgs, JsResult, JsValue};
use unicode_segmentation::UnicodeSegmentation;

use super::{array, bytes_arg, bytes_to_js, object};
use crate::engine::{OpFn, arg_string, js_str, type_error};

fn op_utf8_encode(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let s = arg_string(args, 0, ctx)?;
    bytes_to_js(s.as_bytes(), ctx)
}

fn op_utf8_decode(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let bytes = bytes_arg(args.get_or_undefined(0), ctx)?;
    let fatal = args.get_or_undefined(1).to_boolean();
    if fatal {
        match std::str::from_utf8(&bytes) {
            Ok(s) => Ok(js_str(s)),
            Err(_) => Err(type_error("The encoded data was not valid for encoding utf-8")),
        }
    } else {
        Ok(js_str(&String::from_utf8_lossy(&bytes)))
    }
}

fn op_utf8_length(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let s = args.get_or_undefined(0).to_string(ctx)?.to_std_string_lossy();
    Ok(JsValue::from(s.len() as f64))
}

fn op_base64_encode(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let bytes = bytes_arg(args.get_or_undefined(0), ctx)?;
    let url = args.get_or_undefined(1).to_boolean();
    let s = if url {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&bytes)
    } else {
        base64::engine::general_purpose::STANDARD.encode(&bytes)
    };
    Ok(js_str(&s))
}

fn op_base64_decode(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let s = arg_string(args, 0, ctx)?;
    let strict = args.get_or_undefined(1).to_boolean();
    let mut cleaned: String = s
        .chars()
        .filter(|c| !c.is_ascii_whitespace())
        .map(|c| match c {
            '-' => '+',
            '_' => '/',
            c => c,
        })
        .collect();
    // Node stops at the first invalid character / padding.
    if let Some(i) = cleaned.find(|c: char| !(c.is_ascii_alphanumeric() || c == '+' || c == '/')) {
        if strict && cleaned[i..].chars().any(|c| c != '=') {
            return Err(crate::js::JsNativeError::error()
                .with_message("The string to be decoded is not correctly encoded.")
                .into());
        }
        cleaned.truncate(i);
    }
    if strict && s.trim_end_matches('=').len() % 4 == 1 {
        return Err(crate::js::JsNativeError::error()
            .with_message("The string to be decoded is not correctly encoded.")
            .into());
    }
    let engine = base64::engine::GeneralPurpose::new(
        &base64::alphabet::STANDARD,
        base64::engine::GeneralPurposeConfig::new()
            .with_decode_padding_mode(base64::engine::DecodePaddingMode::Indifferent)
            .with_decode_allow_trailing_bits(true),
    );
    // Drop a dangling single character (cannot encode a full byte).
    if cleaned.len() % 4 == 1 {
        cleaned.pop();
    }
    let bytes = engine.decode(cleaned.as_bytes()).unwrap_or_default();
    bytes_to_js(&bytes, ctx)
}

fn url_parts(u: &url::Url, ctx: &mut Context) -> JsResult<JsValue> {
    let host = match (u.host_str(), u.port()) {
        (Some(h), Some(p)) => format!("{h}:{p}"),
        (Some(h), None) => h.to_string(),
        _ => String::new(),
    };
    let search = match u.query() {
        Some(q) if !q.is_empty() => format!("?{q}"),
        _ => String::new(),
    };
    let hash = match u.fragment() {
        Some(f) if !f.is_empty() => format!("#{f}"),
        _ => String::new(),
    };
    let origin = u.origin().ascii_serialization();
    object(
        ctx,
        vec![
            ("href", js_str(u.as_str())),
            ("protocol", js_str(&format!("{}:", u.scheme()))),
            ("username", js_str(u.username())),
            ("password", js_str(u.password().unwrap_or(""))),
            ("host", js_str(&host)),
            ("hostname", js_str(u.host_str().unwrap_or(""))),
            ("port", js_str(&u.port().map(|p| p.to_string()).unwrap_or_default())),
            ("pathname", js_str(u.path())),
            ("search", js_str(&search)),
            ("hash", js_str(&hash)),
            ("origin", js_str(&origin)),
        ],
    )
}

fn op_url_parse(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let href = arg_string(args, 0, ctx)?;
    let base = args.get_or_undefined(1);
    let parsed = if base.is_undefined() {
        url::Url::parse(&href)
    } else {
        let base = base.to_string(ctx)?.to_std_string_lossy();
        url::Url::parse(&base).and_then(|b| b.join(&href))
    };
    match parsed {
        Ok(u) => url_parts(&u, ctx),
        Err(_) => Ok(JsValue::null()),
    }
}

fn op_url_set(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let href = arg_string(args, 0, ctx)?;
    let field = arg_string(args, 1, ctx)?;
    let value = arg_string(args, 2, ctx)?;
    let Ok(mut u) = url::Url::parse(&href) else { return Ok(JsValue::null()) };
    match field.as_str() {
        "href" => match url::Url::parse(&value) {
            Ok(n) => u = n,
            Err(_) => return Err(type_error(format!("Invalid URL: {value}"))),
        },
        "protocol" => {
            let _ = u.set_scheme(value.trim_end_matches(':'));
        }
        "username" => {
            let _ = u.set_username(&value);
        }
        "password" => {
            let _ = u.set_password(if value.is_empty() { None } else { Some(&value) });
        }
        "host" => {
            let (h, p) = match value.rfind(':') {
                Some(i) if i + 1 < value.len() && !value[i..].contains(']') && value[i + 1..].chars().all(|c| c.is_ascii_digit()) => {
                    (value[..i].to_string(), value[i + 1..].parse::<u16>().ok())
                }
                _ => (value.clone(), None),
            };
            if u.set_host(Some(&h)).is_ok() && p.is_some() {
                let _ = u.set_port(p);
            }
        }
        "hostname" => {
            let _ = u.set_host(Some(&value));
        }
        "port" => {
            if value.is_empty() {
                let _ = u.set_port(None);
            } else if let Ok(p) = value.parse::<u16>() {
                let _ = u.set_port(Some(p));
            }
        }
        "pathname" => u.set_path(&value),
        "search" => {
            let q = value.strip_prefix('?').unwrap_or(&value);
            u.set_query(if q.is_empty() { None } else { Some(q) });
        }
        "hash" => {
            let f = value.strip_prefix('#').unwrap_or(&value);
            u.set_fragment(if f.is_empty() { None } else { Some(f) });
        }
        _ => {}
    }
    url_parts(&u, ctx)
}

fn op_graphemes(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let s = arg_string(args, 0, ctx)?;
    let items: Vec<JsValue> = s.graphemes(true).map(js_str).collect();
    Ok(array(ctx, items))
}

fn op_words(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let s = arg_string(args, 0, ctx)?;
    let items: Vec<JsValue> = s.split_word_bounds().map(js_str).collect();
    Ok(array(ctx, items))
}

fn op_domain_to_ascii(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let s = arg_string(args, 0, ctx)?;
    let out = url::Url::parse(&format!("http://{s}/"))
        .ok()
        .and_then(|u| u.host_str().map(String::from))
        .unwrap_or_default();
    Ok(js_str(&out))
}

pub(crate) const OPS: &[(&str, usize, OpFn)] = &[
    ("utf8Encode", 1, op_utf8_encode),
    ("utf8Decode", 2, op_utf8_decode),
    ("utf8Length", 1, op_utf8_length),
    ("base64Encode", 2, op_base64_encode),
    ("base64Decode", 2, op_base64_decode),
    ("urlParse", 2, op_url_parse),
    ("urlSet", 3, op_url_set),
    ("graphemes", 1, op_graphemes),
    ("words", 1, op_words),
    ("domainToASCII", 1, op_domain_to_ascii),
];
