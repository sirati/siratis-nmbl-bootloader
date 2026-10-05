{ pkgs }:
let console = import ../../../lib/rescue/console.nix { inherit pkgs; };
in pkgs.runCommand "nmbl-console-inherited-state-fixture" {
  nativeBuildInputs = [ pkgs.rustc pkgs.stdenv.cc ];
  PRODUCTION_CONSOLE = "${console}/bin/nmbl-rescue-console";
  FIXTURE_STTY = "${pkgs.coreutils}/bin/stty";
} ''
  mkdir -p "$out/bin"
  rustc --edition=2021 -D warnings ${./main.rs} -o "$out/bin/nmbl-rescue-console"
''
