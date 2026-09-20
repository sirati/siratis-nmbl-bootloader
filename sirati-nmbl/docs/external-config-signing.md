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

For an off-host signer, set `signing.deferInstallSigning = true`. Copy the
unsigned configuration from the target boot filesystem to the signing host,
then run:

```console
nmbl-sign sign \
  --key /secure/off-host/nmbl.key \
  --domain boot-config \
  --out config.toml.sig \
  config.toml
```

Install the configuration and sidecar together at their advertised paths.
Until a matching sidecar exists, the enforcing boot path refuses the external
configuration. The embedded public key remains the trust anchor; neither the
private key nor its contents are Nix derivation inputs.
