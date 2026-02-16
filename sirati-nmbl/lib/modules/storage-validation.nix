# Storage Driver Validation Module
# Detects required storage drivers from filesystem device paths and validates their presence

{ lib }:

rec {
  # Helper function to determine required storage drivers from device path
  # Returns a list of required drivers (some devices need multiple drivers)
  getRequiredDriversForDevice =
    device:
    if device == null then
      [ ]
    else if lib.hasPrefix "/dev/vd" device then
      # VirtIO devices need either virtio_blk or virtio_scsi, PLUS virtio_pci
      [
        {
          driver = "virtio_pci";
          alternatives = [ ];
        }
        {
          driver = "virtio_blk";
          alternatives = [ "virtio_scsi" ];
        }
      ]
    else if lib.hasPrefix "/dev/nvme" device then
      [
        {
          driver = "nvme";
          alternatives = [ ];
        }
      ]
    else if lib.hasPrefix "/dev/sd" device || lib.hasPrefix "/dev/sr" device then
      [
        {
          driver = "sd_mod";
          alternatives = [ ];
        }
      ]
    else if lib.hasPrefix "/dev/hd" device then
      [
        {
          driver = "ahci";
          alternatives = [ ];
        }
      ]
    else if lib.hasPrefix "/dev/md" device then
      [
        {
          driver = "raid";
          alternatives = [ ];
        }
      ]
    else if lib.hasPrefix "/dev/mapper/" device then
      [
        {
          driver = "dm_mod";
          alternatives = [ ];
        }
      ]
    else
      [ ];

  # Collect all required storage driver requirements from filesystems
  # Result is a list of { driver, alternatives, devices } where:
  # - driver: the primary driver name
  # - alternatives: list of alternative drivers that can satisfy this requirement
  # - devices: list of devices that need this driver
  getRequiredStorageDrivers =
    fileSystems:
    let
      # Get all driver requirements with their source devices
      allRequirements = lib.flatten (
        map (
          fs: map (req: req // { device = fs.device; }) (getRequiredDriversForDevice fs.device)
        ) fileSystems
      );

      # Group by driver name
      grouped = lib.groupBy (req: req.driver) allRequirements;

      # Convert to list of unique requirements
      uniqueReqs = lib.mapAttrsToList (driver: reqs: {
        inherit driver;
        alternatives = (lib.head reqs).alternatives;
        devices = map (r: r.device) reqs;
      }) grouped;
    in
    uniqueReqs;

  # Generate assertion for a storage driver requirement
  makeStorageDriverAssertion = config: req: {
    assertion =
      let
        # Check if driver or any alternative is in kernelModules
        # NMBL doesn't have udev, so only explicitly loaded modules (kernelModules) work
        # availableKernelModules are included in the initramfs but won't auto-load
        checkDriver = drv: lib.elem drv config.boot.initrd.kernelModules;

        primaryLoaded = checkDriver req.driver;
        anyAlternativeLoaded = lib.any checkDriver req.alternatives;
        isLoaded = primaryLoaded || anyAlternativeLoaded;
      in
      isLoaded;

    message =
      let
        devices = lib.concatStringsSep ", " (lib.unique req.devices);
        driverInfo = {
          virtio_pci = "VirtIO PCI bus (required for all VirtIO devices)";
          virtio_blk = "VirtIO block devices";
          virtio_scsi = "VirtIO SCSI devices (alternative to virtio_blk)";
          nvme = "NVMe drives";
          sd_mod = "SCSI/SATA drives";
          ahci = "AHCI SATA controller";
          raid = "Software RAID";
          dm_mod = "Device mapper (LVM/LUKS)";
        };
        hint = driverInfo.${req.driver} or "storage driver";

        alternativesText =
          if req.alternatives == [ ] then
            ""
          else
            "\n            Or use an alternative: ${lib.concatStringsSep " or " req.alternatives}";

        solutionDrivers =
          if req.alternatives == [ ] then
            ''boot.initrd.kernelModules = [ "${req.driver}" ];''
          else
            ''boot.initrd.kernelModules = [ "${req.driver}" ];  # or [ "${lib.head req.alternatives}" ]'';
      in
      ''
        NMBL: Missing required storage driver '${req.driver}' for devices: ${devices}

        The '${req.driver}' kernel module is required to access these block devices.
        NMBL doesn't have udev, so storage drivers must be explicitly loaded via boot.initrd.kernelModules.
        Modules in boot.initrd.availableKernelModules are included but won't auto-load.${alternativesText}

        Solution: Add to your NixOS configuration:
          ${solutionDrivers}

        For VirtIO (VMs), you need BOTH virtio_pci AND (virtio_blk or virtio_scsi):
          boot.initrd.kernelModules = [ "virtio_pci" "virtio_blk" ];

        Or include the qemu-guest profile and add storage drivers:
          imports = [ "''${nixpkgs}/nixos/modules/profiles/qemu-guest.nix" ];
          boot.initrd.kernelModules = [ "virtio_pci" "virtio_blk" ];

        To disable this check (not recommended):
          boot.nmbl.ignoreMissingDiskModules = true;

        Note: ${hint}
      '';
  };
}
