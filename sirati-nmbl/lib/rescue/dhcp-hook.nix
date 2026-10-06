# dhcpcd's script in the rescue system: resolv.conf from DHCP, DHCPv6 and
# router advertisements, without the stock bash hooks running as root on
# network input.
{ pkgs }:
pkgs.runCommand "nmbl-rescue-dhcp-hook" { nativeBuildInputs = [ pkgs.rustc pkgs.stdenv.cc ]; } ''
  rustc --edition=2021 -D warnings --test ${./dhcp-hook-rs/main.rs} -o dhcp-hook-tests
  ./dhcp-hook-tests
  mkdir -p $out/bin
  rustc --edition=2021 -D warnings -O ${./dhcp-hook-rs/main.rs} -o $out/bin/nmbl-rescue-dhcp-hook
''
