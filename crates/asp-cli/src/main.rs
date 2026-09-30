//! RASP command-line executable.
//!
//! Milestone 2: `serve` hosts an ASP application over HTTP, `run`
//! renders one page against a synthetic GET request, and `check`
//! syntax-checks every `.asp` file under a directory. The language
//! engine now covers the M2 core subset (arrays, procedures, Exit,
//! conversions, Date/Time) on top of M1's page model.

use asp_core::{AppRoot, AspError};
use asp_http::{ServerConfig, serve as http_serve};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "rasp",
    version,
    about = "RASP: a Classic ASP interpreter in Rust"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run the HTTP server for an ASP application.
    Serve {
        /// Application root directory (contains the .asp pages).
        #[arg(long, default_value = ".")]
        root: PathBuf,
        /// TCP port to listen on.
        #[arg(long, default_value_t = 8174)]
        port: u16,
        /// Address to bind.
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        /// Default document for directory URLs.
        #[arg(long, default_value = "default.asp")]
        default_document: String,
    },
    /// Run a single ASP page against a synthetic request.
    Run {
        /// Page path relative to the application root.
        file: String,
        /// Application root directory.
        #[arg(long, default_value = ".")]
        root: PathBuf,
        /// Query string to expose via Request.QueryString.
        #[arg(long, default_value = "")]
        query: String,
    },
    /// Check all ASP files in a directory for syntax errors.
    Check {
        /// Application root directory to scan.
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Print build and feature information.
    Version,
}

fn main() {
    let cli = Cli::parse();

    let exit = match cli.command {
        Commands::Version => {
            println!("rasp {}", env!("CARGO_PKG_VERSION"));
            0
        }
        Commands::Serve {
            root,
            port,
            host,
            default_document,
        } => match serve(&root, &host, port, &default_document) {
            Ok(()) => 0,
            Err(err) => {
                eprintln!("rasp serve: {err}");
                1
            }
        },
        Commands::Run { file, root, query } => match run_page(&root, &file, &query) {
            Ok(body) => {
                print!("{body}");
                0
            }
            Err(err) => {
                eprintln!("rasp run: {err}");
                1
            }
        },
        Commands::Check { path } => match check(&path) {
            Ok(0) => {
                println!("all pages OK");
                0
            }
            Ok(broken) => {
                println!("{broken} page(s) with errors");
                1
            }
            Err(err) => {
                eprintln!("rasp check: {err}");
                1
            }
        },
    };
    std::process::exit(exit);
}

fn serve(
    root: &std::path::Path,
    host: &str,
    port: u16,
    default_document: &str,
) -> Result<(), AspError> {
    let app = app_root(root)?;
    let config = ServerConfig {
        host: host.to_string(),
        port,
        default_document: default_document.to_string(),
    };
    println!(
        "rasp: serving {} on http://{}:{}",
        app.path().display(),
        host,
        port
    );
    http_serve(&app, &config)
}

fn run_page(root: &std::path::Path, file: &str, query: &str) -> Result<String, AspError> {
    let app = app_root(root)?;
    let data = asp_runtime::build_request_data(query, "", "");
    let out = asp_runtime::render_page(&app, file, data)?;
    Ok(out.body)
}

fn check(root: &std::path::Path) -> Result<usize, AspError> {
    let _app = app_root(root)?;
    let mut broken = 0usize;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir)
            .map_err(|e| AspError::Io(format!("cannot read {}: {e}", dir.display())))?;
        for entry in entries {
            let entry = entry.map_err(|e| AspError::Io(e.to_string()))?;
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if name.to_ascii_lowercase().ends_with(".asp") {
                let source = std::fs::read_to_string(&path)
                    .map_err(|e| AspError::Io(format!("cannot read {}: {e}", path.display())))?;
                match asp_core::Page::parse(&source) {
                    Ok(page) => {
                        // Parse every <% %> block body with the language engine.
                        for block in &page.blocks {
                            if let asp_core::parser::Block::Script { body } = block
                                && let Err(err) = asp_vbscript::parse_block(body, 1)
                            {
                                broken += 1;
                                println!("{}: {err}", path.display());
                            }
                        }
                    }
                    Err(err) => {
                        broken += 1;
                        println!("{}: {err}", path.display());
                    }
                }
            }
        }
    }
    Ok(broken)
}

fn app_root(root: &std::path::Path) -> Result<AppRoot, AspError> {
    let canonical = root
        .canonicalize()
        .map_err(|e| AspError::Io(format!("cannot resolve {}: {e}", root.display())))?;
    Ok(AppRoot::new(canonical))
}
