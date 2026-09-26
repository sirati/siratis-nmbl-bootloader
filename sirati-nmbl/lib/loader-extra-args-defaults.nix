# Defaults for `bootstrapper.loader_extra_args` when a bios/uefi config leaves
# it null. Mirrors the submodule option defaults in lib/options.nix so every
# consumer can read the fields without an `or` fallback.
{
  timeout = 0;
  canTouchEfiVariables = false;
  efiInstallAsRemovable = false;
  efiStubInstallPath = "EFI/BOOT/BOOTX64.EFI";
  default = "0";
  configurationLimit = 100;
  extraConfig = "";
  extraEntries = "";
  theme = null;
}
