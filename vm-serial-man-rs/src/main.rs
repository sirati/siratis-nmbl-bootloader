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
use cli::{BootModeArgs, Cli, Commands, DisplayArg};
use manager::{screenshot, BootMode, Display};

fn display_from_arg(arg: DisplayArg) -> Display {
    match arg {
        DisplayArg::Serial => Display::Serial,
        DisplayArg::Sdl => Display::Sdl,
        DisplayArg::Vnc { port } => Display::Vnc { port },
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging: HH:MM:SS LEVEL (default: info)
    log::init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Manager { config, boot_mode } => {
            let boot_mode = match boot_mode {
                BootModeArgs::Bios => BootMode::Bios,
                BootModeArgs::Uefi {
                    ovmf_code,
                    ovmf_vars,
                } => BootMode::Uefi {
                    ovmf_code,
                    ovmf_vars,
                },
                BootModeArgs::DirectKernel {
                    kernel,
                    initrd,
                    kernel_args,
                } => BootMode::DirectKernel {
                    kernel,
                    initrd,
                    kernel_args,
                },
            };
            let display = display_from_arg(config.display);
            manager::run_manager(
                config.name,
                config.disk,
                boot_mode,
                config.memory,
                config.cores,
                config.socket,
                config.buffer_lines,
                config.buffer_seconds,
                display,
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
        Commands::Tail { lines, socket } => client::get_tail(lines, socket).await,
        Commands::Screenshot {
            output,
            monitor_socket,
        } => {
            screenshot::capture(&monitor_socket, &output)
                .map_err(|e| anyhow::anyhow!("screenshot failed: {e}"))?;
            Ok(())
        }
    }
}
