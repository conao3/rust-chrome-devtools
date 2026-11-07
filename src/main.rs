mod batch;
mod cdp;
mod cli;
mod config;
mod daemon;
mod error;
mod frame;
mod grammar;
mod lock;
mod output;
mod router;

#[cfg(test)]
mod tests;

use std::io::Write;

fn main() {
    let code = match cli::run() {
        Ok(()) => 0,
        Err(error) => {
            let _ = writeln!(std::io::stderr(), "error: {error}");
            error.exit_code()
        }
    };
    std::process::exit(code);
}
