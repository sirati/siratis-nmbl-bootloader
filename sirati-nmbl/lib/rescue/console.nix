{ pkgs }:
pkgs.runCommand "nmbl-rescue-console" { nativeBuildInputs = [ pkgs.rustc pkgs.stdenv.cc ]; RESCUE_TEST_BASH = "${pkgs.bash}/bin/bash"; RESCUE_STTY = "${pkgs.coreutils}/bin/stty"; } ''
  rustc --edition=2021 -D warnings --test ${./console-rs/main.rs} -o console-tests
  ./console-tests
  mkdir -p $out/bin
  rustc --edition=2021 -D warnings ${./console-rs/main.rs} -o $out/bin/nmbl-rescue-console
''
