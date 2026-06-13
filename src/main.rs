mod config;
mod server;
mod event_loop;
mod request;
mod response;
mod router;
mod handler;
mod cgi;
mod session;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: {} <config_file>", args[0]);
        std::process::exit(1);
    }

    let config = match config::parse_config(&args[1]) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Config error: {}", e);
            std::process::exit(1);
        }
    };

    if let Err(e) = server::run(config) {
        eprintln!("Server error: {}", e);
        std::process::exit(1);
    }
}
