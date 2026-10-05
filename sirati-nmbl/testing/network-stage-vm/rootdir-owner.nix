# Test-only ownership metadata for a rootless, runtime-created Btrfs fixture.
{ pkgs }:
pkgs.runCommand "nmbl-fixture-rootdir-owner" { nativeBuildInputs = [ pkgs.stdenv.cc ]; } ''
  mkdir -p "$out/lib"
  $CC -Wall -Wextra -Werror -shared -fPIC -o "$out/lib/rootdir-owner.so" ${./rootdir-owner.c} -ldl
''
