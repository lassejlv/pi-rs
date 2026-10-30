//! Exported constants and factories whose values are pinned to upstream (pi-mono 5e63241):
//! `CODEMODE_TOOL_NAME`, `DEFAULT_MAX_OUTPUT_TOKENS`, `DEFAULT_HARNESS_IDENTITY`, `createPhotonImages`.
use base64::{Engine, engine::general_purpose::STANDARD};
use pi_durable::{
    codemode::{output::DEFAULT_MAX_OUTPUT_TOKENS, types::CODEMODE_TOOL_NAME},
    default_harness_identity,
    images::create_photon_images,
    tools::image_processor::{DEFAULT_IMAGE_LIMITS, ImageProcessor},
};

#[test]
fn code_mode_names_and_limits_equal_upstream() {
    assert_eq!(CODEMODE_TOOL_NAME, "codemode");
    assert_eq!(DEFAULT_MAX_OUTPUT_TOKENS, 10_000.0);
}

#[test]
fn the_default_harness_identity_attributes_requests_to_pi() {
    // `{ attribution: PI_ATTRIBUTION }`: provider attribution to pi, nothing else set.
    assert_eq!(
        serde_json::to_value(default_harness_identity()).unwrap(),
        serde_json::json!({ "attribution": { "name": "pi", "url": "https://pi.dev", "categories": ["cli-agent"] } })
    );
}

#[tokio::test]
async fn the_photon_processor_returns_a_decodable_inline_image_as_it_is() {
    // A 1x1 GIF, as in upstream's images-wasm-module test: returned unchanged only once it decodes.
    let data = "R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7";
    let bytes = STANDARD.decode(data).unwrap();
    let prepared = create_photon_images()
        .prepare(bytes.clone(), "image/gif".into(), DEFAULT_IMAGE_LIMITS)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (prepared.data.as_str(), prepared.mime_type.as_str()),
        (data, "image/gif")
    );
    // Bytes that are not an image prepare to nothing.
    let none = create_photon_images()
        .prepare(
            b"not an image".to_vec(),
            "image/png".into(),
            DEFAULT_IMAGE_LIMITS,
        )
        .await
        .unwrap();
    assert!(none.is_none());
}
