//! QEMU process management
//!
//! This module handles starting and monitoring QEMU processes with serial via Unix socket

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::Stdio;
use tokio::fs;
use tokio::process::{Child, Command};
use tracing::{debug, trace};

/// Boot mode for QEMU
#[derive(Debug, Clone)]
pub enum BootMode {
    /// Boot with UEFI firmware (OVMF)
    Uefi {
        ovmf_code: PathBuf,
        ovmf_vars: PathBuf,
    },
    /// Direct kernel boot (bypass bootloader)
    DirectKernel {
        kernel: PathBuf,
        initrd: PathBuf,
        kernel_args: String,
    },
    /// Boot with legacy BIOS (SeaBIOS)
    Bios,
}

/// Display backend for QEMU.
///
/// - `Serial`: headless (`-nographic`), the historical default.
/// - `Vnc { port }`: expose a VNC server on `port`. QEMU's `-display vnc=:N`
///   syntax uses display numbers, where `N = port - 5900`, so `port` must be
///   at least `5900`.
/// - `Sdl`: open a local SDL window.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Display {
    #[default]
    Serial,
    Vnc {
        port: u16,
    },
    Sdl,
}

/// QEMU configuration
pub struct QemuConfig {
    pub name: String,
    pub disk: PathBuf,
    pub boot_mode: BootMode,
    pub memory: u32,
    pub cores: u32,
    pub socket_dir: PathBuf,
    pub display: Display,
}

/// QEMU process with socket information
pub struct QemuProcess {
    pub child: Child,
    pub serial_socket: PathBuf,
    pub monitor_socket: PathBuf,
}

impl QemuConfig {
    /// Start QEMU and return the process with socket path
    pub async fn start(&self) -> Result<QemuProcess> {
        debug!("Starting QEMU VM: {}", self.name);

        // Handle UEFI boot mode - ensure OVMF vars is writable
        if let BootMode::Uefi {
            ovmf_code,
            ovmf_vars,
        } = &self.boot_mode
        {
            if !ovmf_vars.exists() {
                let vars_template = ovmf_code.parent().unwrap().join("OVMF_VARS.fd");
                fs::copy(&vars_template, ovmf_vars)
                    .await
                    .context("Failed to create OVMF_VARS")?;

                // Ensure the file is writable
                let mut perms = fs::metadata(ovmf_vars).await?.permissions();
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    perms.set_mode(0o644);
                }
                fs::set_permissions(ovmf_vars, perms).await?;
            }
        }

        // Create socket paths for serial and monitor — per-PID to avoid
        // collisions when multiple managers run in parallel (matches the
        // control socket convention).
        let pid = std::process::id();
        let serial_socket = self.socket_dir.join(format!("qemu-serial-{pid}.sock"));
        let monitor_socket = self.socket_dir.join(format!("qemu-monitor-{pid}.sock"));

        // Remove old sockets if they exist
        if serial_socket.exists() {
            fs::remove_file(&serial_socket).await?;
        }
        if monitor_socket.exists() {
            fs::remove_file(&monitor_socket).await?;
        }

        let mut cmd = Command::new("qemu-system-x86_64");

        cmd.arg("-machine")
            .arg("q35,accel=kvm:tcg")
            .arg("-cpu")
            .arg("max")
            .arg("-m")
            .arg(self.memory.to_string())
            .arg("-smp")
            .arg(self.cores.to_string());

        // Add boot-mode-specific arguments
        match &self.boot_mode {
            BootMode::Uefi {
                ovmf_code,
                ovmf_vars,
            } => {
                debug!("Using UEFI boot mode");
                cmd.arg("-drive")
                    .arg(format!(
                        "if=pflash,format=raw,readonly=on,file={}",
                        ovmf_code.display()
                    ))
                    .arg("-drive")
                    .arg(format!("if=pflash,format=raw,file={}", ovmf_vars.display()));
            }
            BootMode::DirectKernel {
                kernel,
                initrd,
                kernel_args,
            } => {
                debug!("Using direct kernel boot mode");
                cmd.arg("-kernel")
                    .arg(kernel)
                    .arg("-initrd")
                    .arg(initrd)
                    .arg("-append")
                    .arg(kernel_args);
            }
            BootMode::Bios => {
                debug!("Using legacy BIOS boot mode");
                // No additional arguments needed - QEMU uses SeaBIOS by default
            }
        }

        cmd.arg("-drive")
            .arg(format!(
                "file={},format=qcow2,if=virtio",
                self.disk.display()
            ))
            .arg("-netdev")
            .arg("user,id=net0")
            .arg("-device")
            .arg("virtio-net-pci,netdev=net0");

        // Dispatch on display backend.
        match self.display {
            Display::Serial => {
                debug!("Using headless (serial) display");
                cmd.arg("-nographic");
            }
            Display::Vnc { port } => {
                let display_num = port
                    .checked_sub(5900)
                    .with_context(|| format!("VNC port {port} must be >= 5900"))?;
                debug!("Using VNC display on port {port} (display :{display_num})");
                cmd.arg("-display")
                    .arg(format!("vnc=:{display_num}"))
                    .arg("-vga")
                    .arg("std");
            }
            Display::Sdl => {
                debug!("Using SDL display");
                cmd.arg("-display").arg("sdl").arg("-vga").arg("std");
            }
        }

        // Serial always goes to its Unix socket; the QEMU monitor gets its own
        // Unix socket so the `screenshot` subcommand can drive `screendump`.
        cmd.arg("-serial")
            .arg(format!("unix:{},server,nowait", serial_socket.display()))
            .arg("-monitor")
            .arg(format!(
                "unix:{},server,nowait",
                monitor_socket.display()
            ));

        // Print the actual QEMU command for debugging
        eprintln!("=== QEMU Command ===");
        eprint!("{:?}", cmd.as_std().get_program());
        for arg in cmd.as_std().get_args() {
            eprint!(" {:?}", arg);
        }
        eprintln!();
        eprintln!("====================");

        let child = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .context("Failed to spawn QEMU")?;

        debug!(
            "QEMU started with serial socket: {} (monitor: {})",
            serial_socket.display(),
            monitor_socket.display()
        );

        Ok(QemuProcess {
            child,
            serial_socket,
            monitor_socket,
        })
    }
}
