//! RASP command-line executable.
//!
//! Milestone 0 skeleton: the binary starts and reports its version along
//! with the subcommands that land in later milestones.

use clap::{Parser, Subcommand};

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
    Serve,
    /// Run a single ASP page against a synthetic request.
    Run { file: String },
    /// Check all ASP files in a directory for syntax errors.
    Check { path: String },
    /// Print build and feature information.
    Version,
}

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Commands::Serve | Commands::Run { .. } | Commands::Check { .. } => {
            eprintln!(
                "this subcommand is not implemented yet; see docs/asp-classic-interpreter-plan.md"
            );
            std::process::exit(2);
        }
        Commands::Version => {
            println!("rasp {}", env!("CARGO_PKG_VERSION"));
        }
    }
}
