# External configuration signing

With `boot.nmbl.configLocation = "external"` (the default is `"embedded"`),
NMBL's initramfs carries only `/etc/nmbl/bootstrap.toml`. That file names the
boot filesystem (`bootstrap.bootFs`) and the path of the full runtime config
on it, `bootstrap.configPath` (default `/nmbl/config.toml`). NMBL mounts the
boot filesystem at `bootstrap.bootFs.mountpoint` (default `/mnt/boot`) and
reads the config from there.

When `boot.nmbl.signing.enable = true`, the bootstrap file also names the
detached signature `<configPath><sigPathSuffix>`. NMBL opens the config once,
hashes it, and verifies the signature under the `nmbl:boot-config:v1` ML-DSA
domain with the public keys baked into `nmbl-init`. It parses the file only
after that check passes. This check always enforces, also in audit mode,
because the signing policy in the external config is untrusted until the
file verifies. A missing or invalid signature stops the boot with a bootstrap
error.

## Install-time signing

The bootloader installer copies the config to `/boot/<configPath>` and signs
it under the `boot-config` domain. The key comes from one of two options:

| Option | Form |
| --- | --- |
| `signing.generationKeyFile` | Path of the private key, read at install time. |
| `signing.generationKeyCommand` | argv that prints the key. The installer runs it once per signature and pipes the output into `nmbl-sign sign --key-stdin`. |

The two options are mutually exclusive. Pass `generationKeyFile` as a string
path outside `/nix/store`; evaluation fails when it resolves into the store.
Only the installer reads the key. Neither the key nor its contents are
derivation inputs. With `signing.deferInstallSigning = true`, the installer
stages the config unsigned and prints a warning, and the signature must be
produced out of band.

## Generation-image hosts

On hosts with `generationImage.enable`, the installer stages no external
config at all. Point the bootstrap config into the active generation:

```nix
boot.nmbl.bootstrap.configPath = "/nmbl-generations/active/config.toml";
boot.nmbl.signing.deferInstallSigning = true;
```

`nmbl-erofs-deploy remote` builds the unsigned config and generation image,
signs both on the operator machine, and streams them to the restricted
receiver. The receiver verifies the `boot-config` and `generation-image`
signatures against its fixed public-key argument before activation. The
bootstrap filesystem must be the filesystem that contains
`generationImage.stateRoot`, so the path above points into the same
generation directory as `nix.erofs`. One rename of `active` therefore
switches the config and the image together. Give the update SSH key only that
receiver command, through a forced command and a fixed sudo rule. See
[erofs-generations.md](erofs-generations.md#deployment).

An existing installation needs one last bootloader update that embeds the
public key and the generation `configPath`. After that, runtime config and
generation updates use the restricted stream. The private key stays off the
target, and the NMBL kernel and initrd stay unchanged.

## Boot-set hosts

With `boot.nmbl.bootUpdate.enable`, GRUB passes
`nmbl.config=/nmbl-boot-sets/<slot>/config`. NMBL uses that path in place of
`configPath` and verifies the sidecar at the same path plus `.sig`. See
[boot-set-updates.md](boot-set-updates.md).

## Manual off-host signing

Copy the unsigned config to the signing host and run:

```console
nmbl-sign sign \
  --key /secure/off-host/nmbl.key \
  --domain boot-config \
  --out config.toml.sig \
  config.toml
```

The key can also arrive on stdin, so it does not have to exist as a file on
the signing host:

```console
nix-secrets pipe-secret nmbl-generation-key \
  | nmbl-sign sign --key-stdin --domain boot-config --out config.toml.sig config.toml
```

Install the config and the sidecar together at their configured paths. Until
a matching sidecar exists, NMBL refuses the external config. The baked public
keys are the trust anchor.
