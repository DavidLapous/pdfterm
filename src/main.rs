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

    /// Print the licenses supplied with the embedded PDFium binary.
    #[arg(long)]
    third_party_licenses: bool,

    /// Resolve a source position and send it to an already-running viewer.
    #[arg(long, conflicts_with = "synctex_view")]
    forward_search: Option<PathBuf>,

    /// Resolve a source position and print the editor-neutral forward JSON request.
    #[arg(long)]
    synctex_view: Option<PathBuf>,

    /// One-based source line for --forward-search or --synctex-view.
    #[arg(long, default_value_t = 1)]
    line: u32,

    /// One-based Unicode character column in the source.
    #[arg(long, default_value_t = 1)]
    column: u32,

    /// Named viewer/editor session sharing the same configuration.
    #[arg(long)]
    session: Option<String>,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    if cli.third_party_licenses {
        return match std::io::Write::write_all(
            &mut std::io::stdout().lock(),
            include_bytes!(concat!(env!("OUT_DIR"), "/pdfium-notices.txt")),
        ) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("pdfterm: {error}");
                ExitCode::FAILURE
            }
        };
    }
    let mut config = match pdfterm::config::Config::load() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("pdfterm: {error}");
            return ExitCode::FAILURE;
        }
    };
    if let Some(name) = cli.session.as_deref()
        && let Err(error) = config.select_session(name)
    {
        eprintln!("pdfterm: {error}");
        return ExitCode::FAILURE;
    }
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
    if let Some(source) = cli.forward_search.as_ref().or(cli.synctex_view.as_ref()) {
        let result = (|| -> Result<(), Box<dyn std::error::Error>> {
            let pdf = cli
                .path
                .as_ref()
                .ok_or("a PDF path is required for forward search")?;
            let request = pdfterm::synctex::resolve_forward(pdf, source, cli.line, cli.column)?;
            if cli.synctex_view.is_some() {
                println!("{}", serde_json::to_string(&request)?);
            } else {
                pdfterm::ipc::forward(
                    config
                        .forward_socket()
                        .ok_or("forward_socket is disabled")?,
                    &request,
                )?;
            }
            Ok(())
        })();
        return match result {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("pdfterm: {error}");
                ExitCode::FAILURE
            }
        };
    }
    match pdfterm::app::run(cli.path, cli.pdfium_library, cli.page - 1, &config) {
        Ok(()) | Err(pdfterm::app::AppError::Quit) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("pdfterm: {error}");
            ExitCode::FAILURE
        }
    }
}
