//! `json.*`: strict-JSON copies through `pi_json::strict::Value` (`js/chord/index.js` `copyJson`).

use crate::host::Methods;
use crate::value::HostValue;

pub(super) fn register(m: &mut Methods) {
    // The glue has already checked prototypes, descriptors and cycles (which the host conversion
    // cannot see); this rejects non-finite numbers and undefined, and returns an alias-free copy.
    m.sync("json.copy", |_, args| {
        let strict = if args.get(1).as_bool() == Some(true) {
            args.get(0).to_strict_omitting_undefined()?
        } else {
            args.get(0).to_strict()?
        };
        Ok(HostValue::from_strict(&strict))
    });
}
