{
  description = "VM Serial Manager - Rust-based QEMU serial I/O manager";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, rust-overlay, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs {
          inherit system overlays;
        };

        rustToolchain = pkgs.rust-bin.stable.latest.default.override {
          extensions = [ "rust-src" "rust-analyzer" ];
        };

        nativeBuildInputs = with pkgs; [
          rustToolchain
          pkg-config
        ];

        buildInputs = with pkgs; [
          openssl
        ];

        vm-serial-man = pkgs.rustPlatform.buildRustPackage {
          pname = "vm-serial-man";
          version = "0.1.0";

          src = ./.;

          cargoLock = {
            lockFile = ./Cargo.lock;
          };

          inherit nativeBuildInputs buildInputs;

          meta = with pkgs.lib; {
            description = "VM Serial Manager for QEMU";
            homepage = "https://github.com/yourusername/vm-serial-man-rs";
            license = licenses.mit;
            maintainers = [ ];
          };
        };

      in
      {
        packages = {
          default = vm-serial-man;
          vm-serial-man = vm-serial-man;
        };

        apps = {
          default = {
            type = "app";
            program = "${vm-serial-man}/bin/vm-serial-man";
          };
        };

        devShells.default = pkgs.mkShell {
          inherit buildInputs;
          nativeBuildInputs = nativeBuildInputs ++ (with pkgs; [
            # Additional development tools
            cargo-watch
            cargo-edit
            cargo-outdated
            clippy
            rustfmt
          ]);
          
          # DO NOT ADD MORE HERE
          shellHook = ''
            echo "Rust version: $(rustc --version)"
            echo "Cargo version: $(cargo --version)"
          '';

          RUST_SRC_PATH = "${rustToolchain}/lib/rustlib/src/rust/library";
        };
      }
    );
}
OVMF