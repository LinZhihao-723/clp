//! `clp-s-rs`: the command-line front end of the Rust clp-s reader.

use std::process::ExitCode;

fn main() -> ExitCode {
    clp_s_cli::run()
}
