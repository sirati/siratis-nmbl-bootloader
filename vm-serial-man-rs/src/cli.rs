//! CLI command definitions for VM Serial Manager

use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;
use std::str::FromStr;

/// CLI representation of [`crate::manager::Display`]. Parsed from strings such
/// as `serial`, `sdl`, or `vnc:5901`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DisplayArg {
    #[default]
    Serial,
    Sdl,
    Vnc {
        port: u16,
    },
}

impl FromStr for DisplayArg {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let lower = s.to_ascii_lowercase();
        match lower.as_str() {
            "serial" => Ok(Self::Serial),
            "sdl" => Ok(Self::Sdl),
            _ => {
                if let Some(rest) = lower.strip_prefix("vnc:") {
                    let port: u16 = rest
                        .parse()
                        .map_err(|e| format!("invalid VNC port {rest:?}: {e}"))?;
                    if port < 5900 {
                        return Err(format!("VNC port {port} must be >= 5900"));
                    }
                    Ok(Self::Vnc { port })
                } else {
                    Err(format!(
                        "expected one of 'serial', 'sdl', 'vnc:PORT', got {s:?}"
                    ))
                }
            }
        }
    }
}

/// CLI representation of the emulated-TPM front-end model. Parsed from `tis`
/// or `crb`; defaults to TIS.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TpmKindArg {
    #[default]
    Tis,
    Crb,
}

impl FromStr for TpmKindArg {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "tis" => Ok(Self::Tis),
            "crb" => Ok(Self::Crb),
            other => Err(format!("expected 'tis' or 'crb', got {other:?}")),
        }
    }
}

#[derive(Parser)]
#[command(name = "vm-serial-man")]
#[command(about = "VM Serial Manager - Manage QEMU VM serial I/O", long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

/// Boot mode specific arguments
#[derive(Subcommand)]
pub enum BootModeArgs {
    /// Boot with UEFI firmware (OVMF)
    Uefi {
        /// Path to OVMF code
        #[arg(long)]
        ovmf_code: PathBuf,

        /// Path to OVMF vars
        #[arg(long)]
        ovmf_vars: PathBuf,
    },

    /// Boot with legacy BIOS (SeaBIOS)
    Bios,

    /// Direct kernel boot (bypass bootloader)
    DirectKernel {
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
    },
}

/// Common manager configuration
#[derive(Args)]
pub struct ManagerConfig {
    /// Name of the VM
    #[arg(short, long, default_value = "test-vm")]
    pub name: String,

    /// Path to disk image
    #[arg(short, long)]
    pub disk: PathBuf,

    /// Memory size in MB
    #[arg(short, long, default_value = "1024")]
    pub memory: u32,

    /// Number of CPU cores
    #[arg(short, long, default_value = "4")]
    pub cores: u32,

    /// Control socket path (auto-generated if not specified)
    #[arg(long)]
    pub socket: Option<PathBuf>,

    /// Buffer size (number of lines)
    #[arg(long, default_value = "10")]
    pub buffer_lines: usize,

    /// Buffer time window (seconds)
    #[arg(long, default_value = "10")]
    pub buffer_seconds: u64,

    /// Display backend: `serial` (headless, default), `sdl`, or `vnc:PORT`
    /// where `PORT` is a TCP port >= 5900.
    #[arg(long, default_value = "serial")]
    pub display: DisplayArg,

    /// Attach an emulated TPM 2.0 backed by an swtpm sidecar. The value is the
    /// PER-RUN state directory (created fresh on start, removed on stop). When
    /// omitted, the VM has no TPM and the QEMU invocation is unchanged.
    #[arg(long)]
    pub tpm: Option<PathBuf>,

    /// TPM front-end model when `--tpm` is set: `tis` (default) or `crb`.
    #[arg(long, default_value = "tis")]
    pub tpm_kind: TpmKindArg,

    /// Persist the swtpm state across this manager's lifetime: do not wipe the
    /// `--tpm` state dir on start, nor remove it on stop. Used to carry a
    /// TPM-sealed secret from a first (enroll) run into a second (unseal) run
    /// of the SAME state dir for a measured-boot seal/unseal roundtrip. A fresh
    /// QEMU power-on still issues `TPM2_Startup(CLEAR)`, so PCRs reset and the
    /// boot re-extends the same deterministic event sequence. No effect without
    /// `--tpm`. The caller owns deleting the state dir when the roundtrip ends.
    #[arg(long, requires = "tpm")]
    pub tpm_persist: bool,

    /// Secure-Boot OVMF code firmware (read-only pflash). Setting both this and
    /// `--sb-vars` enables a Secure-Boot-enforcing machine (`smm=on`),
    /// overriding the boot-mode's own UEFI firmware.
    #[arg(long, requires = "sb_vars")]
    pub sb_code: Option<PathBuf>,

    /// Writable, db-enrolled Secure-Boot OVMF VARS copy (read-write pflash).
    /// Used with `--sb-code` to enable Secure-Boot enforcement.
    #[arg(long, requires = "sb_code")]
    pub sb_vars: Option<PathBuf>,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Start the VM manager daemon
    Manager {
        #[command(flatten)]
        config: ManagerConfig,

        #[command(subcommand)]
        boot_mode: BootModeArgs,
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

    /// Capture the current VM framebuffer to a PPM file via the QEMU monitor
    Screenshot {
        /// Destination path for the PPM file
        output: PathBuf,

        /// QEMU monitor Unix socket of the running VM
        #[arg(long)]
        monitor_socket: PathBuf,
    },
}
