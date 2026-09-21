{ pkgs, lib, cfg, loaderArgs }:

pkgs.writeText "nmbl-grub.cfg" ''
  set timeout=${toString loaderArgs.timeout}
  set default=${loaderArgs.default}
  ${loaderArgs.extraConfig}

  menuentry "NMBL Bootloader" {
  ${if cfg.bootUpdate.enable or false then ''
    # The fixed GRUB image is only a dispatcher. Mutable boot material lives
    # in complete A/B sets, and a missing or malformed selector fails closed.
    search --file --set=nmblroot /nmbl-boot-sets/active
    set root=$nmblroot
    if [ -f /nmbl-boot-sets/active ]; then
      source /nmbl-boot-sets/active
    fi
    if [ "$nmbl_slot" = "A" -o "$nmbl_slot" = "B" ]; then
      if [ -f /nmbl-boot-sets/$nmbl_slot/bootloader ]; then
        source /nmbl-boot-sets/$nmbl_slot/bootloader
      fi
      if [ "$nmbl_kernel" = "kernel" -a "$nmbl_initrd" = "initrd" -a "$nmbl_config" = "config" ]; then
        linux /nmbl-boot-sets/$nmbl_slot/$nmbl_kernel ${lib.concatStringsSep " " cfg.kernelParams} nmbl.config=/nmbl-boot-sets/$nmbl_slot/$nmbl_config
        initrd /nmbl-boot-sets/$nmbl_slot/$nmbl_initrd
      else
        echo "Invalid NMBL boot-set metadata; refusing legacy fallback"
      fi
    else
      echo "Missing or invalid NMBL boot-set selector; refusing legacy fallback"
    fi
  '' else ''
    linux /nmbl-kernel ${lib.concatStringsSep " " cfg.kernelParams}
    initrd /nmbl-initrd
  ''}
  }
  ${loaderArgs.extraEntries}
''
