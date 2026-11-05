//! Reload extension code through the registry: replace, wrap and uninstall by name.
//! Port of upstream test/examples/09-registry-reload.ts.
#[path = "ex_support/mod.rs"]
mod support;
use pi_durable::*;
use std::sync::Arc;
use support::*;

fn files(grep_description: &str, read: &Arc<NativeTool>) -> Arc<Ext> {
    let mut extension = define_extension(Ext::new("files"));
    extension.tools = vec![read.clone(), path_tool("grep", grep_description)];
    Arc::new(extension)
}

#[tokio::main]
async fn main() -> Result<()> {
    let context = Context::default();
    let read = path_tool("read", "Read a file");
    let registry = create_registry::<NativeTool, NativeSection>()?;
    registry.install(files("Search files", &read))?;
    let harness = Harness::open(
        Arc::new(MemoryStorage::new()),
        HarnessOptions::new(registry.clone(), pi_ai::models::Models::default()),
        &context,
    )
    .await?;
    let root = harness
        .root(ConversationOptions::default(), &context)
        .await?;
    let tools = || async { tool_descriptions(&root.agent(&context).await?) };

    let before = tools().await?;
    assert_eq!(before, ["read: Read a file", "grep: Search files"]);

    // Reloading is installing a new object with the same name: it replaces the old one in place.
    registry.install(files("Search files, faster", &read))?;
    let reloaded = tools().await?;
    println!("after reload: {reloaded:?}");
    assert_eq!(
        reloaded,
        ["read: Read a file", "grep: Search files, faster"]
    );

    // Another extension may wrap a tool by name. The wrap applies where the wrapping extension is
    // selected, and survives reloads of the wrapped tool.
    let mut audit = define_extension(Ext::new("audit"));
    audit.wraps.push(wrap_tool(read.as_ref(), |tool| {
        let mut audited = (*tool).clone();
        audited.declaration.description =
            format!("{} (audited)", audited.declaration.description).into();
        Ok(Arc::new(audited))
    }));
    let audit = Arc::new(audit);
    registry.install(audit.clone())?;
    let audited = tools().await?;
    println!("with audit: {audited:?}");
    assert_eq!(
        audited,
        ["read: Read a file (audited)", "grep: Search files, faster"]
    );
    registry.install(files("Search files, much faster", &read))?;
    assert_eq!(
        tools().await?,
        [
            "read: Read a file (audited)",
            "grep: Search files, much faster"
        ],
        "the wrap survives a reload of the wrapped tool"
    );

    // Uninstall removes an extension by name; conversations stop getting its code.
    registry.uninstall(&audit.name)?;
    let removed = tools().await?;
    println!("after uninstall: {removed:?}");
    assert_eq!(
        removed,
        ["read: Read a file", "grep: Search files, much faster"]
    );

    harness.close(&context).await
}
