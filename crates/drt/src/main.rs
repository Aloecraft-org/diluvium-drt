//! The `drt` binary. One call: the command surface lives in the library
//! (`drt::cli`) so a page's terminal can parse the same command line.

use clap::Parser;

fn main() -> std::process::ExitCode {
    drt::cli::main(drt::cli::Cli::parse())
}
