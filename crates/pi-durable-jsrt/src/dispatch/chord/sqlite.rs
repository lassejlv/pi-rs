//! `sqlite.*`: schema validation over `pi_chord::sqlite` (`js/chord/sqlite/*.js`).

use crate::errors::{ErrorClass, HostError};
use crate::host::Methods;
use crate::value::HostValue;

pub(super) fn register(m: &mut Methods) {
    m.sync("sqlite.validateSchemaPrefix", |_, args| {
        pi_chord::sqlite::validate_sqlite_schema_prefix(&args.text(0)?)
            .map_err(|error| HostError::new(ErrorClass::Error, error.to_string()))?;
        Ok(HostValue::Undefined)
    });
}
