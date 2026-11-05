//! The `pi` binary (port of `packages/coding-agent/src/cli.ts`).

fn main() {
    pi_coding_agent::utils::memory::configure_allocator();
    // `child_process.fork()` in extensions re-executes this binary as a Node-like script runner.
    if let Some(code) = pi_jsrt::node::main_if_requested() {
        std::process::exit(code);
    }
    pi_coding_agent::cli::setup::setup_cli();
    let args: Vec<String> = pi_coding_agent::cli::setup::process_args().iter().skip(1).cloned().collect();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime");
    let result = runtime.block_on(pi_coding_agent::main(&args, None));
    if let Err(err) = result {
        eprintln!("{err:#}");
        pi_coding_agent::utils::process_exit::exit(1);
    }
    // Like Node, exit with `process.exitCode` once main() has finished. Background tasks
    // (watchers, servers started by extensions) must not keep the process alive.
    let code = pi_coding_agent::utils::process_exit::exit_code().unwrap_or(0);
    pi_coding_agent::utils::process_exit::exit(code);
}
