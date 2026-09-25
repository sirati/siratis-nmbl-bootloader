# External configuration signing

With `boot.nmbl.configLocation = "external"`, NMBL keeps its full runtime
configuration at `boot.nmbl.bootstrap.configPath` on `/boot`. When
`boot.nmbl.signing.enable = true`, the embedded bootstrap configuration also
names a detached `<configPath><sigPathSuffix>` signature. Early boot verifies
the external configuration under the distinct `nmbl:boot-config:v1` ML-DSA
domain before parsing any of its bytes.

The installer signs the staged file with `signing.generationKeyFile`. Supply
that option as a string path outside `/nix/store`; evaluation rejects a store
path. The private key contents are read only by the imperative bootloader
installation step.

For generation-image hosts, set `signing.deferInstallSigning = true` and use a
versioned config path:

```nix
boot.nmbl.bootstrap.configPath = "/nmbl-generations/active/config.toml";
```

`nmbl-erofs-deploy remote` builds the unsigned config and generation image,
signs both on the operator host, and streams them to the restricted receiver.
The receiver verifies the `boot-config` and `generation-image` domains against
its fixed public-key argument before activation. The bootstrap filesystem must
be the same filesystem that holds `generationImage.stateRoot`; the relative
path above points into the same generation directory as `nix.erofs`. The
single `active` rename therefore switches config and image atomically. The
update SSH key should have only that exact receiver command through a forced
command and a fixed sudo rule.

An existing installation needs one final bootloader-image update to embed the
public key and the versioned bootstrap `configPath`. After that enrollment,
ordinary runtime-config and generation updates use the restricted stream and
do not place the private key on the target or rebuild the immutable boot image.

For a manual off-host signing flow, copy the unsigned configuration to the
signing host, then run:

```console
nmbl-sign sign \
  --key /secure/off-host/nmbl.key \
  --domain boot-config \
  --out config.toml.sig \
  config.toml
```

The key can also arrive on stdin, so it never has to exist as a file on the
signing host:

```console
nix-secrets pipe-secret nmbl-generation-key \
  | nmbl-sign sign --key-stdin --domain boot-config --out config.toml.sig config.toml
```

The install-time signer uses the same pipe when
`boot.nmbl.signing.generationKeyCommand` is set instead of `generationKeyFile`.

Install the configuration and sidecar together at their advertised paths.
Until a matching sidecar exists, the enforcing boot path refuses the external
configuration. The embedded public key remains the trust anchor; neither the
private key nor its contents are Nix derivation inputs.
