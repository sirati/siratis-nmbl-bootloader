# `nmbl-tpm-enroll` — a HOST / install-time helper that seals a LUKS2 volume
# key to the TPM so the box auto-unlocks ONLY on an untampered measured boot.
#
# RULING (master-plan-v2 §A SEALING-REUSE / §F-SOUND): sealing reuses the
# existing `systemd-cryptenroll` + `cryptsetup --token-only` machinery with NO
# Rust `TPM2_Unseal`. This is therefore a `writeShellApplication` wrapper, NOT a
# new Rust binary. It is a HOST tool: it runs once, AFTER the box has booted the
# installed system, against the on-disk LUKS header — it MUST NEVER ship inside
# the NMBL initramfs (the boot path only ever UNSEALS, via `--token-only`, and
# never enrolls). The absence-from-initramfs assertion that backstops this lives
# in `lib/config.nix` (the `nmblTpmEnrollAbsenceCheck`).
#
# TOKEN-FORMAT MATCH (the critical correctness point). NMBL's boot-time TPM
# unlock runs, verbatim from `lib/modules/activation.nix`:
#
#     cryptsetup open --token-only <device> <name>
#
# `--token-only` makes libcryptsetup walk the LUKS2 header tokens and unlock
# from a token's key material WITHOUT a passphrase prompt. `systemd-cryptenroll
# --tpm2-device=…` writes exactly such a token: a LUKS2 `systemd-tpm2` token
# whose handler is built into libcryptsetup (the libcryptsetup ↔ systemd token
# plugin), so a stock `cryptsetup --token-only` consumes it directly. The
# enrolled keyslot holds the random TPM-sealed volume key; cryptsetup unseals it
# through the token at boot iff the bound PCRs match. So `nmbl-tpm-enroll`
# produces precisely the token NMBL's existing `--token-only` unlock consumes.
#
# DEFAULT PCR SET: 11+7. PCR 11 is NMBL's measure PCR (`tpm.pcrIndex`, the lock
# PCR NMBL extends the boot handoff into and caps on a refuse); PCR 7 is the
# firmware / Secure-Boot-state PCR. Binding both means the secret unseals only on
# an untampered measured boot under enforcing Secure Boot — and capping PCR 11 on
# entry to rescue makes the unseal FAIL, keeping secrets safe.

{ pkgs, lib }:

let
  # The default PCR set the tool seals against when `--pcrs` is omitted. Kept in
  # one place so the help text, the flag default, and the docs agree. 11 = NMBL
  # measure PCR (security-consts `lockPcr`); 7 = firmware / Secure-Boot state.
  defaultPcrs = "11+7";
in
pkgs.writeShellApplication {
  name = "nmbl-tpm-enroll";

  runtimeInputs = [
    pkgs.systemd # systemd-cryptenroll (writes the systemd-tpm2 LUKS2 token)
    pkgs.cryptsetup # cryptsetup luksDump / token inspection
    pkgs.coreutils
    pkgs.gnugrep
  ];

  # `writeShellApplication` runs the body under `set -euo pipefail` and
  # shellchecks it at build time, so a non-clean script fails the eval.
  text = ''
    # nmbl-tpm-enroll — seal a LUKS2 volume key to the TPM, bound to the
    # measured-boot PCRs, so NMBL auto-unlocks ONLY on an untampered boot.
    #
    # Round trip: this tool ENROLLS (seals) a systemd-tpm2 token into the LUKS2
    # header; at boot NMBL UNLOCKS it with `cryptsetup open --token-only`, which
    # succeeds iff the sealed PCRs (default ${defaultPcrs}) still match. Entering
    # rescue caps PCR 11, so the unseal then FAILS and the secret stays safe.

    prog=nmbl-tpm-enroll
    pcrs=${defaultPcrs}
    device=""
    keyfile=""
    wipe_existing=0
    tpm_device="auto"

    usage() {
      cat <<EOF
    $prog — seal a LUKS2 volume key to the TPM (measured-boot PCRs)

    USAGE:
      $prog --device <LUKS-DEVICE> [OPTIONS]

    OPTIONS:
      --device <DEV>     LUKS2 block device to enroll (e.g. /dev/disk/by-partlabel/disk-main-luks).
      --pcrs <SET>       TPM2 PCR set to seal against (systemd-cryptenroll syntax,
                         e.g. "11+7"). Default: ${defaultPcrs}
                         (11 = NMBL measure PCR, 7 = firmware/Secure-Boot state).
      --key-file <FILE>  Existing volume key / passphrase file used to authorise the
                         new TPM keyslot. Omit to be prompted for an existing passphrase.
      --tpm2-device <D>  TPM2 device passed to systemd-cryptenroll. Default: auto.
      --wipe-existing    Remove any prior systemd-tpm2 token/keyslot before enrolling
                         (re-enroll after the measured inputs changed).
      -h, --help         Show this help.

    The enrolled token is a LUKS2 'systemd-tpm2' token consumed at boot by NMBL's
    existing 'cryptsetup open --token-only <device> <name>' unlock — no Rust
    TPM2_Unseal, no passphrase prompt when the bound PCRs match.
    EOF
    }

    die() { echo "$prog: error: $*" >&2; exit 1; }

    while [ "$#" -gt 0 ]; do
      case "$1" in
        --device)      [ "$#" -ge 2 ] || die "--device needs an argument"; device="$2"; shift 2 ;;
        --device=*)    device="''${1#*=}"; shift ;;
        --pcrs)        [ "$#" -ge 2 ] || die "--pcrs needs an argument"; pcrs="$2"; shift 2 ;;
        --pcrs=*)      pcrs="''${1#*=}"; shift ;;
        --key-file)    [ "$#" -ge 2 ] || die "--key-file needs an argument"; keyfile="$2"; shift 2 ;;
        --key-file=*)  keyfile="''${1#*=}"; shift ;;
        --tpm2-device) [ "$#" -ge 2 ] || die "--tpm2-device needs an argument"; tpm_device="$2"; shift 2 ;;
        --tpm2-device=*) tpm_device="''${1#*=}"; shift ;;
        --wipe-existing) wipe_existing=1; shift ;;
        -h|--help)     usage; exit 0 ;;
        *)             die "unknown argument: $1 (try --help)" ;;
      esac
    done

    [ -n "$device" ] || { usage >&2; die "--device is required"; }
    [ -b "$device" ] || [ -e "$device" ] || die "device not found: $device"

    # Refuse / warn early if no usable TPM is present: a sealed token would be
    # unconsumable and the enroll itself would fail confusingly. Probe the
    # in-kernel TPM presence the same way NMBL's boot path latches it (sysfs).
    if [ ! -e /sys/class/tpm/tpm0 ] && [ ! -e /dev/tpmrm0 ] && [ ! -e /dev/tpm0 ]; then
      die "no TPM present (no /sys/class/tpm/tpm0, /dev/tpmrm0 or /dev/tpm0). \
    A measured-boot seal needs a TPM2 device. Aborting so no unusable token is written."
    fi

    # Confirm the device is a LUKS2 container — systemd-tpm2 tokens are a LUKS2
    # feature; a LUKS1 header has no token store and 'cryptsetup --token-only'
    # would never find one.
    if ! cryptsetup isLuks --type luks2 "$device" 2>/dev/null; then
      die "$device is not a LUKS2 container (systemd-tpm2 tokens require LUKS2)."
    fi

    if [ "$wipe_existing" -eq 1 ]; then
      echo "$prog: wiping any existing TPM2 keyslot before re-enrolling..."
      # Best-effort: a fresh header has nothing to wipe.
      systemd-cryptenroll --wipe-slot=tpm2 "$device" || \
        echo "$prog: note: no existing tpm2 slot to wipe (continuing)."
    fi

    echo "$prog: sealing the LUKS volume key of $device to the TPM, bound to PCRs $pcrs ..."

    # The seal. systemd-cryptenroll generates a random volume key, seals it to
    # the TPM under the named PCR policy, adds a LUKS2 keyslot for it, and writes
    # a 'systemd-tpm2' token referencing that slot. The token is exactly what
    # NMBL's boot-time 'cryptsetup open --token-only' consumes.
    enroll_args=( --tpm2-device="$tpm_device" --tpm2-pcrs="$pcrs" )
    if [ -n "$keyfile" ]; then
      [ -f "$keyfile" ] || die "key file not found: $keyfile"
      # PASSWORD env is how systemd-cryptenroll consumes an existing unlock
      # secret non-interactively; read the keyfile into it.
      PASSWORD="$(cat -- "$keyfile")"
      export PASSWORD
    fi

    systemd-cryptenroll "''${enroll_args[@]}" "$device" \
      || die "systemd-cryptenroll failed to seal $device to PCRs $pcrs"

    unset PASSWORD 2>/dev/null || true

    # Confirm a consumable token landed in the header so the operator does not
    # discover a non-unlocking box only at the next boot.
    if cryptsetup luksDump "$device" 2>/dev/null | grep -qi 'systemd-tpm2'; then
      echo "$prog: success — a systemd-tpm2 token is present in $device."
      echo "$prog: at boot NMBL will run 'cryptsetup open --token-only $device <name>'"
      echo "$prog: and unlock without a passphrase iff PCRs $pcrs still match."
    else
      die "enroll reported success but no systemd-tpm2 token is visible in $device's header."
    fi
  '';

  meta = {
    description = "Seal a LUKS volume key to the TPM bound to NMBL's measured-boot PCRs (11+7).";
    mainProgram = "nmbl-tpm-enroll";
  };
}
