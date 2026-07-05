mod batch;
mod cdp;
mod cli;
mod config;
mod daemon;
mod lock;
mod router;

#[cfg(test)]
mod tests;

fn main() {
    if let Err(error) = cli::run() {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}
