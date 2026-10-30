//! Ports of `packages/ai/test/*.test.ts` cases that the per-provider test files do not cover.
//! Each module names the TypeScript files it ports; live (credentialed) suites are `#[ignore]`d
//! and gated on the same environment variables as the TypeScript tests.

#[path = "../mock_http/mod.rs"]
mod mock_http;
#[path = "../oauth_support/mod.rs"]
mod oauth_support;
mod support;

mod anthropic;
mod openai_completions;
mod openai_responses;
mod google;
mod bedrock;
mod oauth;
mod catalog;
mod models;
mod errors;
mod misc;
mod transcript;
mod live;
mod providers;
mod cache;
mod runtime;
mod identity;
