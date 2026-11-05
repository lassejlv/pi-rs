//! AWS SDK fidelity checks found by the pi-ai audit: per-request credential resolution, the
//! shared config file cache, nested STS/Signin client requests (user agent, retries, query
//! bodies, role assumer region). These change the process environment, so they live alone in
//! this test binary and run as one test.

mod mock_http;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use mock_http::{MockServer, Request, Response};
use pi_ai::api::aws_credentials;
use pi_ai::api::aws_eventstream::{HeaderValue, encode_message};
use pi_ai::api::bedrock_converse_stream;
use pi_ai::providers::all::get_builtin_model;
use pi_ai::*;
use serde_json::json;

const DPOP_KEY: &str = "-----BEGIN EC PRIVATE KEY-----\nMHcCAQEEIEzHXHjrTwvbMdO4kTDd92PR4YYrI1896OVmsSKg6HYboAoGCCqGSM49\nAwEHoUQDQgAEvX41IxLw4TD31Pih/b+tqxUoQpyAHyZ2Jh8jGXISsVp9ZKlTgcda\nh/HzSVv6r2e3fw7kMuwFPbwnzPlRTwiuPQ==\n-----END EC PRIVATE KEY-----\n";

fn set(name: &str, value: &str) {
    // SAFETY: this binary has a single test; no other thread reads the environment concurrently.
    unsafe { std::env::set_var(name, value) }
}

fn unset(name: &str) {
    // SAFETY: see `set`.
    unsafe { std::env::remove_var(name) }
}

fn ok_stream() -> Vec<u8> {
    let frame = |event_type: &str, payload: serde_json::Value| {
        encode_message(
            &[(":message-type", HeaderValue::String("event".into())), (":event-type", HeaderValue::String(event_type.into()))],
            payload.to_string().as_bytes(),
        )
    };
    let mut body = frame("messageStart", json!({ "role": "assistant" }));
    body.extend(frame("messageStop", json!({ "stopReason": "end_turn" })));
    body
}

fn sts_xml(access: &str) -> String {
    format!(
        "<AssumeRoleResponse><AssumeRoleResult><Credentials><AccessKeyId>{access}</AccessKeyId><SecretAccessKey>s</SecretAccessKey><SessionToken>t</SessionToken><Expiration>2999-01-01T00:00:00Z</Expiration></Credentials></AssumeRoleResult></AssumeRoleResponse>"
    )
}

fn hello() -> TranscriptContext {
    normalize_context(&Context { system_prompt: None, tools: None, messages: vec![Message::User(UserMessage::new("hello", 1))] })
}

fn model_at(url: &str) -> Model {
    let mut model = get_builtin_model("amazon-bedrock", "us.anthropic.claude-opus-4-8").unwrap();
    model.base_url = url.to_string();
    model
}

fn options(profile: &str) -> StreamOptions {
    let mut env = ProviderEnv::new();
    env.insert("AWS_REGION".into(), "us-east-1".into());
    StreamOptions { env: Some(env), cache_retention: Some(CacheRetention::None), ..Default::default() }.with_extra("profile", profile)
}

fn scope_region(request: &Request) -> String {
    let authorization = request.header("authorization").unwrap_or_default();
    authorization.split("Credential=").nth(1).unwrap().split('/').nth(2).unwrap().to_string()
}

#[tokio::test]
async fn aws_sdk_fidelity() {
    for name in [
        "AWS_PROFILE",
        "AWS_REGION",
        "AWS_DEFAULT_REGION",
        "AWS_ACCESS_KEY_ID",
        "AWS_SECRET_ACCESS_KEY",
        "AWS_SESSION_TOKEN",
        "AWS_WEB_IDENTITY_TOKEN_FILE",
        "AWS_ROLE_ARN",
        "AWS_BEARER_TOKEN_BEDROCK",
        "AWS_ENDPOINT_URL",
        "AWS_SDK_UA_APP_ID",
        "AWS_EXECUTION_ENV",
        "AWS_MAX_ATTEMPTS",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "http_proxy",
        "https_proxy",
        "ALL_PROXY",
    ] {
        unset(name);
    }
    let home = std::env::temp_dir().join(format!("pi-ai-aws-fidelity-{}", std::process::id()));
    std::fs::create_dir_all(home.join(".aws")).unwrap();
    set("HOME", home.to_str().unwrap());
    set("AWS_CONFIG_FILE", home.join(".aws/config").to_str().unwrap());
    set("AWS_SHARED_CREDENTIALS_FILE", home.join(".aws/credentials").to_str().unwrap());
    set("AWS_EC2_METADATA_DISABLED", "true");
    aws_credentials::clear_shared_ini_file_cache();
    let write = |path: &Path, text: &str| std::fs::write(path, text).unwrap();
    write(&home.join(".aws/credentials"), "[static]\naws_access_key_id = AKIASTATIC\naws_secret_access_key = static-secret\n");
    write(
        &home.join(".aws/config"),
        "[profile outer]\nrole_arn = arn:aws:iam::1:role/outer\nsource_profile = middle\nregion = us-west-2\n\
[profile middle]\nrole_arn = arn:aws:iam::1:role/middle\nsource_profile = static\nregion = eu-central-1\n\
[profile login]\nlogin_session = arn:aws:iam::1:user/me\nregion = us-east-1\nsdk_ua_app_id = profile-app\n",
    );

    let sts_calls = Arc::new(AtomicUsize::new(0));
    let counter = sts_calls.clone();
    let server = MockServer::start(move |request: &Request, _| {
        let path = request.path_only().to_string();
        if path.starts_with("/model/") {
            return Response::new(200, "application/vnd.amazon.eventstream", ok_stream());
        }
        if path == "/v1/token" {
            return Response::json(
                200,
                json!({ "accessToken": { "accessKeyId": "ASIAREFRESHED", "secretAccessKey": "s", "sessionToken": "t" }, "refreshToken": "r2", "expiresIn": 900 }),
            );
        }
        if path == "/" && request.method == "POST" {
            // The first STS request fails with a throttling error: the SDK retries it.
            if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                return Response::new(
                    400,
                    "text/xml",
                    "<ErrorResponse><Error><Type>Sender</Type><Code>Throttling</Code><Message>Rate exceeded</Message></Error></ErrorResponse>",
                );
            }
            let access = if request.text().contains("role%2Fmiddle") { "ASIAMIDDLE" } else { "ASIAOUTER" };
            return Response::new(200, "text/xml", sts_xml(access));
        }
        Response::text(404, "not found")
    })
    .await;
    set("AWS_ENDPOINT_URL_STS", &server.url);
    set("AWS_ENDPOINT_URL_SIGNIN", &server.url);

    // A chained AssumeRole: one role assumer for the chain, in the outermost profile's region.
    let message = bedrock_converse_stream::stream(&model_at(&server.url), &hello(), options("outer")).result().await;
    assert_eq!(message.stop_reason, StopReason::Stop, "{:?}", message.error_message);
    let sts: Vec<Request> = server.requests().into_iter().filter(|r| r.path == "/").collect();
    assert_eq!(sts.len(), 3, "throttled request retried once, then two AssumeRole calls");
    assert_eq!(sts[0].header("amz-sdk-request"), Some("attempt=1; max=3"));
    assert_eq!(sts[1].header("amz-sdk-request"), Some("attempt=2; max=3"));
    assert_eq!(sts[0].header("amz-sdk-invocation-id"), sts[1].header("amz-sdk-invocation-id"));
    for request in &sts {
        assert_eq!(scope_region(request), "us-west-2");
    }
    let body = sts[1].text();
    assert!(
        body.starts_with("Action=AssumeRole&Version=2011-06-15&RoleArn=arn%3Aaws%3Aiam%3A%3A1%3Arole%2Fmiddle&RoleSessionName=aws-sdk-js-"),
        "{body}"
    );
    assert!(body.ends_with("&DurationSeconds=3600"), "{body}");
    let user_agent = sts[1].header("user-agent").unwrap();
    let release = std::fs::read_to_string("/proc/sys/kernel/osrelease").map(|r| r.trim().to_string()).unwrap_or_default();
    if cfg!(target_os = "linux") {
        assert!(user_agent.starts_with(&format!("aws-sdk-js/3.997.44 ua/2.1 os/linux#{release} lang/js md/nodejs#22.19.0 api/sts#3.997.44 m/E")), "{user_agent}");
    }
    // The STS client's features include its signing credentials' sources: the static profile for
    // the middle role, then the middle role's assumed credentials for the outer one.
    assert!(user_agent.contains(" m/E,n"), "{user_agent}");
    assert!(sts[2].header("user-agent").unwrap().contains(" m/E,i,o"), "{:?}", sts[2].header("user-agent"));
    assert_eq!(sts[1].header("x-amz-user-agent"), Some("aws-sdk-js/3.997.44"));
    let bedrock = server.requests().into_iter().rfind(|r| r.path.starts_with("/model/")).unwrap();
    assert!(bedrock.header("authorization").unwrap().contains("Credential=ASIAOUTER/"));
    if cfg!(target_os = "linux") {
        assert!(bedrock.header("user-agent").unwrap().starts_with(&format!("aws-sdk-js/3.1127.0 ua/2.1 os/linux#{release} lang/js")));
    }
    assert!(bedrock.header("user-agent").unwrap().contains(" m/E,i,o "), "{:?}", bedrock.header("user-agent"));

    // Every request resolves the chain again (a new client per request in TS).
    let message = bedrock_converse_stream::stream(&model_at(&server.url), &hello(), options("outer")).result().await;
    assert_eq!(message.stop_reason, StopReason::Stop, "{:?}", message.error_message);
    assert_eq!(server.requests().iter().filter(|r| r.path == "/").count(), 5);

    // The shared config files are read once per process (`@smithy` readFile cache).
    write(&home.join(".aws/config"), "[profile outer]\nrole_arn = arn:aws:iam::1:role/outer\nsource_profile = static\nregion = ap-south-1\n");
    bedrock_converse_stream::stream(&model_at(&server.url), &hello(), options("outer")).result().await;
    let last_sts = server.requests().into_iter().rfind(|r| r.path == "/").unwrap();
    assert_eq!(scope_region(&last_sts), "us-west-2");
    aws_credentials::clear_shared_ini_file_cache();
    bedrock_converse_stream::stream(&model_at(&server.url), &hello(), options("outer")).result().await;
    let last_sts = server.requests().into_iter().rfind(|r| r.path == "/").unwrap();
    assert_eq!(scope_region(&last_sts), "ap-south-1");

    // `aws login` refresh: the Signin client's user agent (with the profile's app id), SDK
    // request headers and a DPoP proof.
    write(
        &home.join(".aws/config"),
        "[profile login]\nlogin_session = arn:aws:iam::1:user/me\nregion = us-east-1\nsdk_ua_app_id = profile-app\n",
    );
    aws_credentials::clear_shared_ini_file_cache();
    let login_cache = home.join(".aws/login/cache");
    std::fs::create_dir_all(&login_cache).unwrap();
    let login_name: String =
        <sha2::Sha256 as sha2::Digest>::digest(b"arn:aws:iam::1:user/me").iter().map(|b| format!("{b:02x}")).collect();
    write(
        &login_cache.join(format!("{login_name}.json")),
        &json!({
            "accessToken": { "accessKeyId": "ASIAOLD", "secretAccessKey": "s", "sessionToken": "t", "accountId": "1", "expiresAt": "2000-01-01T00:00:00Z" },
            "clientId": "arn:aws:signin:::devtools/same-device",
            "refreshToken": "r",
            "dpopKey": DPOP_KEY
        })
        .to_string(),
    );
    let message = bedrock_converse_stream::stream(&model_at(&server.url), &hello(), options("login")).result().await;
    assert_eq!(message.stop_reason, StopReason::Stop, "{:?}", message.error_message);
    let refresh = server.requests().into_iter().find(|r| r.path == "/v1/token").unwrap();
    assert_eq!(refresh.header("x-amz-user-agent"), Some("aws-sdk-js/3.997.44"));
    let user_agent = refresh.header("user-agent").unwrap();
    assert!(user_agent.contains("api/signin#3.997.44 m/E app/profile-app"), "{user_agent}");
    assert_eq!(refresh.header("amz-sdk-request"), Some("attempt=1; max=3"));
    assert!(refresh.header("amz-sdk-invocation-id").is_some());
    assert!(refresh.header("dpop").is_some());
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&refresh.text()).unwrap(),
        json!({ "clientId": "arn:aws:signin:::devtools/same-device", "grantType": "refresh_token", "refreshToken": "r" })
    );
    let bedrock = server.requests().into_iter().rfind(|r| r.path.starts_with("/model/")).unwrap();
    assert!(bedrock.header("authorization").unwrap().contains("Credential=ASIAREFRESHED/"));

    // `maxAttempts` (NODE_MAX_ATTEMPT_CONFIG_OPTIONS): the profile's `max_attempts`, overridden by
    // `AWS_MAX_ATTEMPTS`; a value `parseInt` cannot read fails the request.
    write(
        &home.join(".aws/config"),
        "[profile login]\nlogin_session = arn:aws:iam::1:user/me\nregion = us-east-1\nmax_attempts = 5\n",
    );
    aws_credentials::clear_shared_ini_file_cache();
    let message = bedrock_converse_stream::stream(&model_at(&server.url), &hello(), options("login")).result().await;
    assert_eq!(message.stop_reason, StopReason::Stop, "{:?}", message.error_message);
    let bedrock = server.requests().into_iter().rfind(|r| r.path.starts_with("/model/")).unwrap();
    assert_eq!(bedrock.header("amz-sdk-request"), Some("attempt=1; max=5"));
    set("AWS_MAX_ATTEMPTS", "4 attempts");
    bedrock_converse_stream::stream(&model_at(&server.url), &hello(), options("login")).result().await;
    let bedrock = server.requests().into_iter().rfind(|r| r.path.starts_with("/model/")).unwrap();
    assert_eq!(bedrock.header("amz-sdk-request"), Some("attempt=1; max=4"));
    set("AWS_MAX_ATTEMPTS", "many");
    let message = bedrock_converse_stream::stream(&model_at(&server.url), &hello(), options("login")).result().await;
    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(
        message.error_message.as_deref(),
        Some("Environment variable AWS_MAX_ATTEMPTS mast be a number, got \"many\"")
    );
    unset("AWS_MAX_ATTEMPTS");

    unset("AWS_ENDPOINT_URL_STS");
    unset("AWS_ENDPOINT_URL_SIGNIN");
    let _ = std::fs::remove_dir_all(&home);
}
