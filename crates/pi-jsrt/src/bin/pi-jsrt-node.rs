//! `pi-jsrt-node [options] <script> [args...]`: run a script like `node` does (see
//! `pi_jsrt::node`). Used as the fork/cluster child in pi-jsrt's own tests; the `pi` binary
//! provides the same mode behind `pi_jsrt::node::NODE_MARKER`.

fn main() {
    if let Some(code) = pi_jsrt::node::main_if_requested() {
        std::process::exit(code);
    }
    std::process::exit(pi_jsrt::node::run(std::env::args().skip(1).collect()));
}
