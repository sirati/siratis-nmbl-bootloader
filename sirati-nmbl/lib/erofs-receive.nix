# The restricted update receiver. It runs as root behind a forced command and
# a sudo rule, so it is the compiled `nmbl-erofs-receive` from nmbl-host-tools
# (built with nmbl-sign), not a script: the stream is parsed, verified and
# installed in-process. `nmblErofsCtl` is accepted for callers that still pass
# it; install and activate are done by the receiver itself.
{ pkgs, nmblSign, nmblErofsCtl ? null }:

pkgs.runCommand "nmbl-erofs-receive" {
  meta.mainProgram = "nmbl-erofs-receive";
} ''
  test -x ${nmblSign}/bin/nmbl-erofs-receive
  mkdir -p $out/bin
  ln -s ${nmblSign}/bin/nmbl-erofs-receive $out/bin/nmbl-erofs-receive
''
