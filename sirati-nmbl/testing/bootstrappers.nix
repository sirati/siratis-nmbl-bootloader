# Bootstrapper sub-axis. These are not a public axis (the spec lists
# only start-mode × target × interaction) but every NMBL config needs
# one of these choices; the registry exists so a start-mode/target
# pair can pick a default and so legacy app names like
# `install-test-gpt-uefi-grub` keep building.
#
# Each entry is the `bootstrapper` attrset consumed by `boot.nmbl =
# { bootstrapper = ...; }` in the NixOS config.
{ ... }:
let
  serialExtraConfig = ''
    serial --unit=0 --speed=115200
    terminal_input serial
    terminal_output serial
  '';
in
{
  gpt-bios = {
    partition_table = "gpt";
    bootMode = "bios";
    loader = "grub";
    loader_extra_args = {
      timeout = 0;
      extraConfig = serialExtraConfig;
    };
  };
  gpt-uefi-grub = {
    partition_table = "gpt";
    bootMode = "uefi";
    loader = "grub";
    loader_extra_args = {
      timeout = 0;
      extraConfig = serialExtraConfig;
    };
  };
  gpt-uefi-systemd = {
    partition_table = "gpt";
    bootMode = "uefi";
    loader = "systemd";
    loader_extra_args = {
      timeout = 0;
    };
  };
  qemu-kernel-invoke = {
    partition_table = "gpt";
    bootMode = "qemu_kernel_invoke";
  };
}
