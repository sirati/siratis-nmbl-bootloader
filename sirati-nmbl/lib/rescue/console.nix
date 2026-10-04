{ pkgs }:
pkgs.runCommand "nmbl-rescue-console" { nativeBuildInputs = [ pkgs.rustc pkgs.stdenv.cc ]; } ''
  mkdir -p $out/bin
  rustc --edition=2021 -D warnings ${./console-rs/main.rs} -o $out/bin/nmbl-rescue-console
''
