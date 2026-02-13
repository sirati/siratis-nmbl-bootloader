//! VM Serial Manager - Rust implementation
//!
//! This tool provides a reliable way to interact with QEMU VMs through serial PTY,
//! with proper buffering and command handling.

use anyhow::Result;

mod buffer;
mod cli;
mod client;
mod log;
mod manager;
mod protocol;

use clap::Parser;
use cli::{Cli, Commands};
use manager::BootMode;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging: HH:MM:SS LEVEL (default: info)
    log::init();

    let cli = Cli::parse();

    match cli.command {
        Commands::ManagerUefi {
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
            let boot_mode = BootMode::Uefi {
                ovmf_code,
                ovmf_vars,
            };
            manager::run_manager(
                name,
                disk,
                boot_mode,
                memory,
                cores,
                socket,
                buffer_lines,
                buffer_seconds,
            )
            .await
        }
        Commands::ManagerDirectKernel {
            name,
            disk,
            kernel,
            initrd,
            kernel_args,
            memory,
            cores,
            socket,
            buffer_lines,
            buffer_seconds,
        } => {
            let boot_mode = BootMode::DirectKernel {
                kernel,
                initrd,
                kernel_args,
            };
            manager::run_manager(
                name,
                disk,
                boot_mode,
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
            min_prev_lines,
            prev_lines_within,
            max_prev_lines,
            socket,
            stdin,
        } => {
            client::send_command(
                command,
                duration,
                min_prev_lines,
                prev_lines_within,
                max_prev_lines,
                socket,
                stdin,
            )
            .await
        }
        Commands::Find {
            pattern,
            before,
            after,
            first,
            last,
            socket,
        } => client::find_in_history(pattern, before, after, first, last, socket).await,
        Commands::Trigger {
            pattern,
            lines_before,
            lines_after,
            match_timeout,
            line_timeout,
            socket,
        } => {
            client::trigger_on_pattern(
                pattern,
                lines_before,
                lines_after,
                match_timeout,
                line_timeout,
                socket,
            )
            .await
        }
        Commands::Attach { socket } => client::attach_console(socket).await,
        Commands::Stop { socket } => client::stop_manager(socket).await,
        Commands::Status => client::show_status().await,
        Commands::Lines {
            start,
            end,
            length,
            socket,
        } => {
            let actual_end = if length { start + end - 1 } else { end };
            client::get_lines(start, actual_end, socket).await
        }
    }
}
