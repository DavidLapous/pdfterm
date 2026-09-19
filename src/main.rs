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

    /// Print the shared configuration as JSON (also creates the default file).
    #[arg(long)]
    print_config: bool,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let config = match pdfterm::config::Config::load() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("pdfterm: {error}");
            return ExitCode::FAILURE;
        }
    };
    if cli.print_config {
        match serde_json::to_string(&config) {
            Ok(json) => {
                println!("{json}");
                return ExitCode::SUCCESS;
            }
            Err(error) => {
                eprintln!("pdfterm: {error}");
                return ExitCode::FAILURE;
            }
        }
    }
    match pdfterm::app::run(cli.path, cli.pdfium_library, cli.page - 1, &config) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("pdfterm: {error}");
            ExitCode::FAILURE
        }
    }
}
