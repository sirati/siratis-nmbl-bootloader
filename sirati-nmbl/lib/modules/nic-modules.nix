# Heuristic NIC-driver detector for the rescue-network path.
#
# Filters `config.boot.initrd.kernelModules` +
# `config.boot.initrd.availableKernelModules` (where
# hardware-configuration.nix records Ethernet/Wi-Fi drivers) down to
# the subset NMBL should explicitly modprobe in the rescue path.
#
# Returns a deduplicated list of module names. Used by lib/config.nix
# only when `boot.nmbl.rescue.network = true`, so non-network builds
# pay no cost.

{ lib, config }:

let
  # Common kernel NIC module names and prefixes. Covers virtio/QEMU,
  # the major Intel/Realtek/Broadcom/Mellanox families, and a handful
  # of cloud-vendor drivers. Intentionally conservative: random
  # storage modules in availableKernelModules must not slip through.
  exactMatches = [
    "virtio_net"
    "vmxnet3"
    "ena"
    "tg3"
    "alx"
    "atl1c"
    "atl1e"
    "atl1"
    "atlantic"
    "sky2"
    "skge"
    "via_rhine"
    "via_velocity"
    "sis190"
    "sis900"
    "ne2k_pci"
    "8139cp"
    "8139too"
    "dl2k"
    "epic100"
    "sundance"
    "tlan"
    "fealnx"
    "pcnet32"
  ];

  prefixMatches = [
    "e100"      # e100, e1000, e1000e
    "igb"       # igb, igbvf
    "ixgb"      # ixgbe, ixgbevf
    "i40e"
    "iavf"
    "ice"
    "r816"      # r8169
    "r812"      # r8125, r8126
    "r815"      # r8152
    "bnx"       # bnx2, bnx2x, bnxt_en
    "mlx"       # mlx4_*, mlx5_*
    "nfp"
    "qed"
    "qede"
    "be2net"
    "cxgb"
    "enic"
    "fm10k"
    "nicvf"
    "thunder"
    "hns"
    "iwl"       # iwlwifi, iwldvm, iwlmvm
    "ath"       # ath9k, ath10k, ath11k, ath12k
    "rtw"       # rtw88, rtw89
    "rtl81"     # rtl8188*, rtl8192*, rtl8723*
    "brcm"      # brcmfmac, brcmsmac
    "mt76"      # mt76*, mt7921, mt7915
    "wl"
  ];

  isNic = m:
    builtins.elem m exactMatches
    || lib.any (p: lib.hasPrefix p m) prefixMatches;

  declared =
    config.boot.initrd.kernelModules
    ++ config.boot.initrd.availableKernelModules;
in
lib.unique (builtins.filter isNic declared)
