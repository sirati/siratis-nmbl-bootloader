# Target: clone of luks-password (single GPT disk, LUKS-on-vda3,
# passphrase "test") but with the GRAPHICAL image-splash enabled.
#
# Identical LUKS activation to luks-password.nix, plus:
#   - boot.nmbl.splash.enable = true  → builds nmbl-init-splash
#     (image-splash cargo feature) and embeds the cosmic-greeter
#     background PNG at /etc/splash/image.png.
#   - DRM graphics drivers in boot.nmbl.earlyKernelModules so
#     /dev/dri/card0 (QEMU virtio-gpu) exists BEFORE the splash
#     console opens. A direct-kernel kexec boot has no EFI GOP, so
#     simpledrm never binds; virtio_gpu is the only DRM device.
#
# This target is VNC-only: the compose layer constrains it to the
# `vnc` interaction because the splash needs a framebuffer, not a UART.
{ pkgs, lib, ... }:
{
  id = "luks-password-splash";
  description = "LUKS-on-vda3 passphrase unlock, GRAPHICAL splash (VNC only)";
  diskoModule = ./disko/luks-password.nix;
  extraInitrdKernelModules = [
    "dm_mod"
    "dm-crypt"
    "aesni_intel"
  ];
  # Linux 6.6 trips a crypto-API init bug in dm-crypt; use latest so
  # NMBL stage-0 can actually open the volume.
  nmblKernelPackage = pkgs.linuxPackages_latest.kernel;
  diskCount = 1;
  extraModules = [
    ({ lib, ... }: {
      # NMBL has no udev; storage drivers have to be explicitly listed
      # in boot.initrd.kernelModules so the bootloader can open the
      # LUKS device.
      boot.initrd.kernelModules = [ "dm_mod" "dm-crypt" "aesni_intel" ];

      # Enable the graphical image-splash binary (nmbl-init-splash) and
      # embed the default cosmic-greeter background PNG.
      boot.nmbl.splash.enable = true;

      # Pre-console graphics drivers (phase 2a). A direct-kernel kexec
      # boot has no EFI GOP so simpledrm won't bind; QEMU's
      # `-device virtio-vga` exposes a virtio-gpu instead. These must
      # be live BEFORE SplashConsole::open so /dev/dri/card0 exists,
      # otherwise the splash falls back to the tty console.
      boot.nmbl.earlyKernelModules = [
        "virtio_pci"
        "virtio_gpu"
        "virtio_dma_buf"
        "drm"
        "drm_kms_helper"
      ];

      boot.nmbl.activation.luks = [
        {
          name = "cryptroot";
          device = "/dev/vda3";
          unlock = "password";
          promptLabel = "Enter LUKS passphrase for cryptroot";
          passToStage1 = "/etc/nmbl-luks/cryptroot";
        }
      ];
      # Tell the post-kexec NixOS initrd to read the injected
      # passphrase instead of prompting. fallbackToPassword keeps
      # the operator able to recover if injection ever fails.
      boot.initrd.luks.devices.cryptroot = lib.mkForce {
        device = "/dev/disk/by-partlabel/disk-main-luks";
        keyFile = "/etc/nmbl-luks/cryptroot";
        fallbackToPassword = true;
        allowDiscards = true;
      };
    })
  ];
}
