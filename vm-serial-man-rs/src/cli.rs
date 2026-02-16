//! CLI command definitions for VM Serial Manager

use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "vm-serial-man")]
#[command(about = "VM Serial Manager - Manage QEMU VM serial I/O", long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Start the VM manager daemon with UEFI boot
    ManagerUefi {
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
        #[arg(long, default_value = "10")]
        buffer_lines: usize,

        /// Buffer time window (seconds)
        #[arg(long, default_value = "10")]
        buffer_seconds: u64,
    },

    /// Start the VM manager daemon with direct kernel boot (fast testing)
    ManagerDirectKernel {
        /// Name of the VM
        #[arg(short, long, default_value = "test-vm")]
        name: String,

        /// Path to disk image
        #[arg(short, long)]
        disk: PathBuf,

        /// Path to kernel binary
        #[arg(short, long)]
        kernel: PathBuf,

        /// Path to initrd/initramfs
        #[arg(short, long)]
        initrd: PathBuf,

        /// Kernel command line arguments
        #[arg(
            long,
            default_value = "console=ttyS0,115200 earlyprintk=serial,ttyS0,115200"
        )]
        kernel_args: String,

        /// Memory size in MB
        #[arg(short, long, default_value = "2048")]
        memory: u32,

        /// Number of CPU cores
        #[arg(short, long, default_value = "4")]
        cores: u32,

        /// Control socket path (auto-generated if not specified)
        #[arg(long)]
        socket: Option<PathBuf>,

        /// Buffer size (number of lines)
        #[arg(long, default_value = "10")]
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

        /// Minimum number of previous lines to show
        #[arg(long, default_value = "10")]
        min_prev_lines: usize,

        /// Time window for previous lines (seconds)
        #[arg(long, default_value = "10")]
        prev_lines_within: u64,

        /// Maximum number of previous lines to show
        #[arg(long, default_value = "30")]
        max_prev_lines: usize,

        /// Control socket path (auto-detected if not specified)
        #[arg(long)]
        socket: Option<PathBuf>,

        /// Additional input lines from stdin
        #[arg(long)]
        stdin: bool,
    },

    /// Search through output history with regex
    Find {
        /// Regex pattern to search for
        pattern: String,

        /// Number of lines before match to show
        #[arg(short, long, default_value = "0")]
        before: usize,

        /// Number of lines after match to show
        #[arg(short, long, default_value = "0")]
        after: usize,

        /// Only return first N matches
        #[arg(long)]
        first: Option<usize>,

        /// Only return last N matches
        #[arg(long)]
        last: Option<usize>,

        /// Control socket path (auto-detected if not specified)
        #[arg(long)]
        socket: Option<PathBuf>,
    },

    /// Trigger on pattern match in new output
    Trigger {
        /// Regex pattern to trigger on
        pattern: String,

        /// Number of lines before match to capture
        #[arg(short = 'b', long, default_value = "0")]
        lines_before: usize,

        /// Number of lines after match to capture
        #[arg(short = 'n', long, default_value = "10")]
        lines_after: usize,

        /// Timeout to wait for pattern match (seconds)
        #[arg(long, default_value = "15")]
        match_timeout: u64,

        /// Timeout to wait for each line after match (seconds)
        #[arg(long, default_value = "5")]
        line_timeout: u64,

        /// Control socket path (auto-detected if not specified)
        #[arg(long)]
        socket: Option<PathBuf>,
    },

    /// Attach to VM console (interactive mode)
    Attach {
        /// Control socket path (auto-detected if not specified)
        #[arg(long)]
        socket: Option<PathBuf>,
    },

    /// Stop a running VM manager
    Stop {
        /// Control socket path (auto-detected if not specified)
        #[arg(long)]
        socket: Option<PathBuf>,
    },

    /// Show status of running VM managers
    Status,

    /// Show specific lines from output history
    Lines {
        /// Starting line number (1-indexed)
        start: usize,

        /// Ending line number (inclusive), or length if --length is used
        end: usize,

        /// Treat 'end' parameter as length instead of end line number
        #[arg(short, long)]
        length: bool,

        /// Control socket path (auto-detected if not specified)
        #[arg(long)]
        socket: Option<PathBuf>,
    },

    /// Show the last N lines from output history (like tail command)
    Tail {
        /// Number of lines to show
        lines: usize,

        /// Control socket path (auto-detected if not specified)
        #[arg(long)]
        socket: Option<PathBuf>,
    },
}
