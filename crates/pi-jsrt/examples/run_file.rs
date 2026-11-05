//! Run a JS/TS module with pi-jsrt and print its default export as JSON:
//! `cargo run --example run_file -- <file> [cwd]`
use pi_jsrt::{JsRuntime, RuntimeOptions};

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    let mut args = std::env::args().skip(1);
    let file = std::path::absolute(args.next().expect("file")).unwrap();
    let cwd = args.next().map(Into::into).or_else(|| file.parent().map(|p| p.to_path_buf()));
    let t0 = std::time::Instant::now();
    let rt = JsRuntime::new(RuntimeOptions { cwd, ..Default::default() }).unwrap();
    eprintln!("startup: {:?}", t0.elapsed());
    match rt.import(file.display().to_string()).await {
        Ok(ns) => println!("{}", ns.get("default").await.map(|v| v.to_string()).unwrap_or_default()),
        Err(e) => {
            eprintln!("{e}\n{}", e.as_js().and_then(|j| j.stack.clone()).unwrap_or_default());
            std::process::exit(1);
        }
    }
    eprintln!("total: {:?}", t0.elapsed());
}
