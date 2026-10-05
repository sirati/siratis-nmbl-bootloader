# Btrfs requests its checksum provider through the crypto API, not modules.dep.
# PID 1 has no userspace modprobe helper, so retain and load it explicitly.
{ lib, kernelVersion, fsType }:
lib.optional (fsType == "btrfs")
  (if lib.versionAtLeast kernelVersion "6.16" then "crc32c-cryptoapi" else "crc32c_generic")
