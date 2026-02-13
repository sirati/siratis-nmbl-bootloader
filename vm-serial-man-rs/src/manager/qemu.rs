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
}

/// QEMU configuration
pub struct QemuConfig {
    pub name: String,
    pub disk: PathBuf,
    pub boot_mode: BootMode,
    pub memory: u32,
    pub cores: u32,
    pub socket_dir: PathBuf,
}

/// QEMU process with socket information
pub struct QemuProcess {
    pub child: Child,
    pub serial_socket: PathBuf,
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

        // Create socket path for serial
        let serial_socket = self.socket_dir.join("qemu-serial.sock");

        // Remove old socket if it exists
        if serial_socket.exists() {
            fs::remove_file(&serial_socket).await?;
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
        }

        cmd.arg("-drive")
            .arg(format!(
                "file={},format=qcow2,if=virtio",
                self.disk.display()
            ))
            .arg("-netdev")
            .arg("user,id=net0")
            .arg("-device")
            .arg("virtio-net-pci,netdev=net0")
            .arg("-nographic")
            .arg("-serial")
            .arg(format!("unix:{},server,nowait", serial_socket.display()))
            .arg("-monitor")
            .arg("none");

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
            "QEMU started with serial socket: {}",
            serial_socket.display()
        );

        Ok(QemuProcess {
            child,
            serial_socket,
        })
    }
}
