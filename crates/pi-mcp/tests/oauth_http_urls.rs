use std::sync::Arc;

use pi_mcp::oauth::{
    AuthorizationServerDiscoveryOptions, AuthorizationServerMetadata, OAuthClientInformation,
    OAuthServerInfoOptions, StartAuthorizationOptions, discover_authorization_server_metadata,
    discover_oauth_server_info, get_search_param, parse_authorization_server_metadata,
    parse_protected_resource_metadata, start_authorization,
};
use pi_mcp::{HttpResponse, McpFetch};
use serde_json::{Value, json};
use url::Url;

const UNSAFE_URLS: &[&str] = &[
    "file://attacker.example/share/x.exe",
    "search-ms:query=x",
    "ms-msdt:/id",
    "custom://localhost/authorize",
    "javascript:alert(1)",
    "data:text/plain,x",
    "vbscript:msgbox(1)",
    "ftp://as.example/authorize",
];

fn metadata() -> Value {
    json!({
        "issuer": "https://as.example",
        "authorization_endpoint": "https://as.example/authorize",
        "token_endpoint": "https://as.example/token",
        "registration_endpoint": "https://as.example/register",
        "response_types_supported": ["code"]
    })
}

fn response(body: Value) -> McpFetch {
    Arc::new(move |_| {
        let body = body.to_string();
        Box::pin(async move { Ok(HttpResponse::from_bytes(200, Default::default(), body)) })
    })
}

#[test]
fn oauth_metadata_urls_allow_only_http_and_https() {
    for value in UNSAFE_URLS {
        for (field, name) in [
            ("issuer", "authorization server issuer"),
            ("authorization_endpoint", "authorization endpoint"),
            ("token_endpoint", "token endpoint"),
            ("registration_endpoint", "registration endpoint"),
        ] {
            let mut input = metadata();
            input[field] = json!(value);
            let error = parse_authorization_server_metadata(input).unwrap_err();
            assert_eq!(error.to_string(), format!("Invalid {name}"), "{value}");
            assert!(!error.is_type_error());
        }
        for (input, name) in [
            (
                json!({"resource": value}),
                "OAuth protected resource metadata resource",
            ),
            (
                json!({"resource": "https://mcp.example", "authorization_servers": [value]}),
                "authorization server URL",
            ),
        ] {
            let error = parse_protected_resource_metadata(input).unwrap_err();
            assert_eq!(error.to_string(), format!("Invalid {name}"), "{value}");
            assert!(!error.is_type_error());
        }
    }
    for scheme in ["http", "https"] {
        let endpoint = format!("{scheme}://as.example/path");
        let mut input = metadata();
        for field in [
            "issuer",
            "authorization_endpoint",
            "token_endpoint",
            "registration_endpoint",
        ] {
            input[field] = json!(endpoint);
        }
        let parsed = parse_authorization_server_metadata(input.clone()).unwrap();
        assert_eq!(serde_json::to_value(parsed).unwrap(), input);
        let input = json!({"resource": endpoint, "authorization_servers": [endpoint]});
        let parsed = parse_protected_resource_metadata(input.clone()).unwrap();
        assert_eq!(serde_json::to_value(parsed).unwrap(), input);
    }
}

#[test]
fn authorization_url_allows_only_http_and_https_including_fallback() {
    for value in UNSAFE_URLS {
        let options = StartAuthorizationOptions {
            metadata: Some(AuthorizationServerMetadata::new(
                "https://as.example",
                *value,
                "https://as.example/token",
            )),
            ..Default::default()
        };
        let error = start_authorization("https://as.example", &options).unwrap_err();
        assert_eq!(
            error.to_string(),
            format!(
                "Invalid authorization endpoint {}",
                Url::parse(value).unwrap()
            )
        );
    }
    for value in ["file:///tmp/server", "custom://localhost/server"] {
        let error = start_authorization(value, &StartAuthorizationOptions::default()).unwrap_err();
        assert_eq!(
            error.to_string(),
            format!(
                "Invalid authorization endpoint {}",
                Url::parse(value).unwrap().join("/authorize").unwrap()
            )
        );
    }
    for scheme in ["http", "https"] {
        let base = format!("{scheme}://as.example/tenant");
        for metadata in [
            None,
            Some(AuthorizationServerMetadata::new(
                &base,
                format!("{base}/login"),
                format!("{base}/token"),
            )),
        ] {
            let expected_path = if metadata.is_some() {
                "/tenant/login"
            } else {
                "/authorize"
            };
            let options = StartAuthorizationOptions {
                metadata,
                client_information: Some(OAuthClientInformation::new("client")),
                redirect_url: "com.example.app:/callback".into(),
                ..Default::default()
            };
            let (url, verifier) = start_authorization(&base, &options).unwrap();
            assert_eq!(url.scheme(), scheme);
            assert_eq!(url.path(), expected_path);
            assert_eq!(
                get_search_param(&url, "client_id").as_deref(),
                Some("client")
            );
            assert_eq!(
                get_search_param(&url, "redirect_uri").as_deref(),
                Some("com.example.app:/callback")
            );
            assert_eq!(
                get_search_param(&url, "code_challenge_method").as_deref(),
                Some("S256")
            );
            assert!(!verifier.is_empty());
        }
    }
}

#[tokio::test]
async fn discovery_rejects_unsafe_endpoints_and_preserves_resource_fallback() {
    for value in UNSAFE_URLS {
        let mut input = metadata();
        input["authorization_endpoint"] = json!(value);
        let error = discover_authorization_server_metadata(
            "https://as.example",
            AuthorizationServerDiscoveryOptions {
                fetch: Some(response(input)),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
        assert_eq!(error.to_string(), "Invalid authorization endpoint");

        let value = value.to_string();
        let fetch: McpFetch = Arc::new(move |request| {
            let body = if request.url.path().contains("oauth-protected-resource") {
                json!({"resource": "https://mcp.example/mcp", "authorization_servers": [value]})
            } else {
                assert_eq!(request.url.host_str(), Some("mcp.example"));
                json!({"issuer": "https://mcp.example", "authorization_endpoint": "https://mcp.example/authorize", "token_endpoint": "https://mcp.example/token", "response_types_supported": ["code"]})
            };
            Box::pin(async move {
                Ok(HttpResponse::from_bytes(
                    200,
                    Default::default(),
                    body.to_string(),
                ))
            })
        });
        let info = discover_oauth_server_info(
            &Url::parse("https://mcp.example/mcp").unwrap(),
            OAuthServerInfoOptions {
                fetch: Some(fetch),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(info.resource_metadata.is_none());
        assert_eq!(info.authorization_server_url, "https://mcp.example/");
    }
}
