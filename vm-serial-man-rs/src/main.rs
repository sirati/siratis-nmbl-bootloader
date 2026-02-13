//! VM Serial Manager - Rust implementation
//!
//! This tool provides a reliable way to interact with QEMU VMs through serial PTY,
//! with proper buffering and command handling.

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

mod manager;
mod client;
mod protocol;
mod buffer;
mod log;

#[derive(Parser)]
#[command(name = "vm-serial-man")]
#[command(about = "VM Serial Manager - Manage QEMU VM serial I/O", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the VM manager daemon
    Manager {
        /// Name of the VM
        #[arg(short, long, default_value = "test-vm")]
        name: String,

        /// Path to disk image
        #[arg(short, long)]
        disk: PathBuf,

        /// Path to OVMF code
        #[arg(long)]
        ovmf_code: PathBuf,

        /// Path to OVMF vars
        #[arg(long)]
        ovmf_vars: PathBuf,

        /// Memory size in MB
        #[arg(short, long, default_value = "1024")]
        memory: u32,

        /// Number of CPU cores
        #[arg(short, long, default_value = "4")]
        cores: u32,

        /// Control socket path (auto-generated if not specified)
        #[arg(long)]
        socket: Option<PathBuf>,

        /// Buffer size (number of lines)
        #[arg(long, default_value = "100")]
        buffer_lines: usize,

        /// Buffer time window (seconds)
        #[arg(long, default_value = "10")]
        buffer_seconds: u64,
    },

    /// Send a command to a running VM manager
    Send {
        /// Command to send
        command: String,

        /// Duration to capture output (seconds)
        #[arg(short, long, default_value = "5")]
        duration: u64,

        /// Control socket path (auto-detected if not specified)
        #[arg(long)]
        socket: Option<PathBuf>,

        /// Additional input lines from stdin
        #[arg(long)]
        stdin: bool,
    },

    /// Stop a running VM manager
    Stop {
        /// Control socket path (auto-detected if not specified)
        #[arg(long)]
        socket: Option<PathBuf>,
    },

    /// Show status of running VM managers
    Status,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging: HH:MM:SS LEVEL (default: info)
    log::init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Manager {
            name,
            disk,
            ovmf_code,
            ovmf_vars,
            memory,
            cores,
            socket,
            buffer_lines,
            buffer_seconds,
        } => {
            manager::run_manager(
                name,
                disk,
                ovmf_code,
                ovmf_vars,
                memory,
                cores,
                socket,
                buffer_lines,
                buffer_seconds,
            )
            .await
        }
        Commands::Send {
            command,
            duration,
            socket,
            stdin,
        } => client::send_command(command, duration, socket, stdin).await,
        Commands::Stop { socket } => client::stop_manager(socket).await,
        Commands::Status => client::show_status().await,
    }
}
