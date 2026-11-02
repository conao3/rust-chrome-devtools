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

fn main() {
    let code = match cli::run() {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("error: {error}");
            error.exit_code()
        }
    };
    std::process::exit(code);
}
