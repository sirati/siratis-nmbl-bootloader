{
  lib,
  cfg,
  deferInstallSigning,
  rescueStageInstaller,
}:

let
  stage = cfg.rescue.fullSystem.networkStage;
  destination = "/boot/${stage.imagePath}";
in
''
  ${lib.optionalString (!deferInstallSigning) ''
    echo "Staging and signing rescue networking image..."
    ${rescueStageInstaller}/bin/nmbl-install-rescue-stage
  ''}
  ${lib.optionalString deferInstallSigning ''
    echo "WARNING: signed rescue-stage installation deferred; run nmbl-install-rescue-stage out of band for ${destination}." >&2
  ''}
''
