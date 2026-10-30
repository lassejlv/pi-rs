//! Feed native-generated operations to the pinned TypeScript reference verifier.
use pi_chord::{JsonValue, delta::diff_revisions};
use serde::Serialize;
#[derive(Serialize)]
struct Row<'a> {
    before: &'a JsonValue,
    after: &'a JsonValue,
    ops: Vec<pi_chord::delta::Op>,
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let fixture: JsonValue = serde_json::from_str(include_str!("../tests/fixtures/delta.json"))?;
    let rows: Vec<_> = fixture["diffs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| Row {
            before: &row["before"],
            after: &row["after"],
            ops: diff_revisions(&row["before"], &row["after"]),
        })
        .collect();
    println!("{}", serde_json::to_string(&rows)?);
    Ok(())
}
