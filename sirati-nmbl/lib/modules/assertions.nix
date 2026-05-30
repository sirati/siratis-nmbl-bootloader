# Configuration Assertions Module
# Validates NMBL bootloader configuration and generates helpful error messages

{
  lib,
  config,
  cfg,
  bootstrapper,
  actualLoader,
  actualLoaderExtraArgs,
  storageValidation,
  nmblFileSystems,
}:

let
  # Get required storage drivers from filesystems
  requiredStorageDrivers = storageValidation.getRequiredStorageDrivers nmblFileSystems;

  # Generate storage driver assertions
  storageDriverAssertions = lib.optionals (!cfg.ignoreMissingDiskModules) (
    map (req: storageValidation.makeStorageDriverAssertion config req) requiredStorageDrivers
  );

in
{
  # All NMBL configuration assertions
  assertions = [
    {
      assertion = bootstrapper.partition_table == "gpt";
      message = "boot.nmbl.bootstrapper.partition_table must be 'gpt' (only GPT is supported)";
    }
    {
      assertion =
        bootstrapper.bootMode == "bios"
        || bootstrapper.bootMode == "uefi"
        || bootstrapper.bootMode == "qemu_kernel_invoke";
      message = "boot.nmbl.bootstrapper.bootMode must be 'bios', 'uefi', or 'qemu_kernel_invoke'";
    }
    {
      assertion =
        actualLoader == null
        || actualLoader == "grub"
        || actualLoader == "systemd"
        || actualLoader == "efi-stub";
      message = "boot.nmbl.bootstrapper.loader must be 'grub', 'systemd', 'efi-stub', or null";
    }
    {
      assertion = bootstrapper.bootMode == "qemu_kernel_invoke" || actualLoader != null;
      message = "loader must be set for bios/uefi boot modes (should default to 'grub')";
    }
    {
      assertion = bootstrapper.bootMode != "qemu_kernel_invoke" || bootstrapper.loader == null;
      message = "loader must not be set when bootMode is 'qemu_kernel_invoke' (QEMU directly invokes kernel, no bootloader needed)";
    }
    {
      assertion = bootstrapper.bootMode != "qemu_kernel_invoke" || bootstrapper.loader_extra_args == null;
      message = "loader_extra_args must not be set when bootMode is 'qemu_kernel_invoke' (no bootloader is used)";
    }
    {
      assertion = actualLoader != "systemd" || bootstrapper.bootMode == "uefi";
      message = "systemd-boot (loader='systemd') requires UEFI boot mode. Use loader='grub' for BIOS boot.";
    }
    {
      assertion = actualLoader != "efi-stub" || bootstrapper.bootMode == "uefi";
      message = "efi-stub (loader='efi-stub') requires UEFI boot mode. The UKI is an EFI executable booted directly by UEFI firmware.";
    }
    {
      assertion =
        bootstrapper.bootMode == "qemu_kernel_invoke"
        || config.fileSystems ? "/boot"
        || (bootstrapper.bootMode == "uefi" && config.fileSystems ? "/efi");
      message = ''
        NMBL requires a separate boot partition (except for qemu_kernel_invoke mode).
        For UEFI boot, declare fileSystems."/boot" or fileSystems."/efi" with fsType = "vfat".
        For BIOS boot, declare fileSystems."/boot" with fsType = "vfat".

        Example:
          fileSystems."/boot" = {
            device = "/dev/sda1";  # or /dev/vda1 for VirtIO
            fsType = "vfat";
          };
      '';
    }
    {
      assertion =
        let
          bootFS =
            if config.fileSystems ? "/boot" then
              config.fileSystems."/boot"
            else if config.fileSystems ? "/efi" then
              config.fileSystems."/efi"
            else
              null;
        in
        (bootstrapper.bootMode == "qemu_kernel_invoke") || (bootFS != null -> bootFS.fsType == "vfat");
      message = "NMBL boot partition must be FAT32 (fsType = \"vfat\")";
    }
    {
      # efi-stub writes a single UKI to the ESP fallback path and never
      # touches NVRAM, so loader_extra_args (and these grub/systemd-only
      # EFI knobs) do not apply — guard the access so the default empty
      # set doesn't trip the lookup.
      assertion =
        bootstrapper.bootMode == "qemu_kernel_invoke"
        || actualLoader == "efi-stub"
        || actualLoaderExtraArgs == null
        || !actualLoaderExtraArgs.efiInstallAsRemovable
        || !actualLoaderExtraArgs.canTouchEfiVariables;
      message = "Cannot use both efiInstallAsRemovable and canTouchEfiVariables. Choose one.";
    }
  ]
  # Add storage driver validation assertions
  ++ storageDriverAssertions;
}
