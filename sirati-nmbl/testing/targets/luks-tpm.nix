# Target: single GPT disk with vda3 wrapped in a LUKS container
# unlocked by a TPM-sealed token. NMBL stage-0 attempts the TPM
# unlock with the activation TPM helper; if no TPM is present the
# unlock fails and stage-0 falls back to the password modal.
#
# The disko layout still seals with the fixed passphrase "test" at
# install time because TPM enrolment must happen *after* the box has
# booted into the installed NixOS; tests that exercise this target
# typically do so against the nixos-anywhere-installed disks where
# enrolment scripts can run.
{ pkgs, lib, ... }:
{
  id = "luks-tpm";
  description = "single GPT disk, LUKS-on-vda3, TPM token unlock (fallback to passphrase)";
  diskoModule = ./disko/luks-password.nix;
  extraInitrdKernelModules = [
    "dm_mod"
    "dm-crypt"
    "aesni_intel"
    "tpm"
    "tpm_tis"
    "tpm_crb"
  ];
  nmblKernelPackage = pkgs.linuxPackages_latest.kernel;
  diskCount = 1;
  extraModules = [
    ({ lib, ... }: {
      boot.initrd.kernelModules = [ "dm_mod" "dm-crypt" "aesni_intel" "tpm" "tpm_tis" "tpm_crb" ];
      boot.nmbl.activation.luks = [
        {
          name = "cryptroot";
          device = "/dev/vda3";
          unlock = "tpm";
          promptLabel = "Enter LUKS passphrase for cryptroot (TPM fallback)";
          passToStage1 = "/etc/nmbl-luks/cryptroot";
        }
      ];
      boot.initrd.luks.devices.cryptroot = lib.mkForce {
        device = "/dev/disk/by-partlabel/disk-main-luks";
        keyFile = "/etc/nmbl-luks/cryptroot";
        fallbackToPassword = true;
        allowDiscards = true;
      };
    })
  ];
}
