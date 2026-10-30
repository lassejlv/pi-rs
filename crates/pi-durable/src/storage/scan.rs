use crate::{Cursor, Error, JsonValue, MAX_SAFE_INTEGER, Result, ScanOrder, wire};

pub struct ScanStart {
    pub order: ScanOrder,
    pub after: Option<i64>,
}
pub fn cursor_id(cursor: Option<&Cursor>) -> Result<Option<i64>> {
    let Some(after) = cursor.and_then(|cursor| wire::get(cursor.value(), "after")) else {
        return Ok(None);
    };
    safe_integer(after)
        .map(Some)
        .ok_or_else(|| Error::request("Invalid storage cursor"))
}
pub fn scan_start(
    requested: Option<ScanOrder>,
    cursor: Option<&Cursor>,
    fallback: ScanOrder,
) -> Result<ScanStart> {
    let Some(cursor) = cursor else {
        return Ok(ScanStart {
            order: requested.unwrap_or(fallback),
            after: None,
        });
    };
    let after = cursor_id(Some(cursor))?.ok_or_else(|| Error::request("Invalid storage cursor"))?;
    let order = match wire::get(cursor.value(), "order") {
        None => fallback,
        Some(value) if value.string_equals("ascending") => ScanOrder::Ascending,
        Some(value) if value.string_equals("descending") => ScanOrder::Descending,
        _ => return Err(Error::request("Invalid storage cursor")),
    };
    if requested.is_some_and(|requested| requested != order) {
        let name = |order| match order {
            ScanOrder::Ascending => "ascending",
            ScanOrder::Descending => "descending",
        };
        return Err(Error::request(format!(
            "The cursor continues a {} scan; the query asks for {}",
            name(order),
            name(requested.unwrap())
        )));
    }
    Ok(ScanStart {
        order,
        after: Some(after),
    })
}
pub fn next_cursor(id: i64, order: ScanOrder) -> Cursor {
    let mut cursor = Cursor::default();
    cursor.insert("after".into(), wire::encode(&id));
    cursor.insert("order".into(), wire::encode(&order));
    cursor
}
fn safe_integer(value: &JsonValue) -> Option<i64> {
    let n = value.as_f64()?;
    (n.is_finite() && n.fract() == 0.0 && n.abs() <= MAX_SAFE_INTEGER as f64).then_some(n as i64)
}
