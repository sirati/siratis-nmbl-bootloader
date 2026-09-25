# Wrap a `writeShellApplication { inheritPath = false; }` tool so it records the
# caller's PATH in NMBL_CALLER_PATH before its own PATH is replaced. The tools
# use it only to run an operator-supplied signing-key command
# (NMBL_SIGN_KEY_COMMAND), which must resolve like it would in the caller's
# shell. An outer wrapper keeps an already-recorded value, so nested tools
# (nmbl-erofs-deploy -> nmbl-erofsctl) keep the original operator PATH.
{ pkgs }:
name: inner:
pkgs.writeShellScriptBin name ''
  export NMBL_CALLER_PATH="''${NMBL_CALLER_PATH:-''${PATH:-}}"
  exec ${inner}/bin/${name} "$@"
''
