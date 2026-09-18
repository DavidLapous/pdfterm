use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    /// PDF file to open. Omit to open the file picker.
    path: Option<PathBuf>,

    /// Path to libpdfium.dylib, libpdfium.so, or pdfium.dll.
    #[arg(long)]
    pdfium_library: Option<PathBuf>,

    /// One-based page number to show first.
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..))]
    page: u32,

    /// Unix socket for forward-search payloads, overriding the config file.
    #[arg(long)]
    forward_socket: Option<String>,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let mut config = pdfterm::config::Config::load();
    if cli.forward_socket.is_some() {
        config.set_forward_socket(cli.forward_socket);
    }
    match pdfterm::app::run(cli.path, cli.pdfium_library, cli.page - 1, &config) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("pdfterm: {error}");
            ExitCode::FAILURE
        }
    }
}
