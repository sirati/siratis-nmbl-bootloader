{
  description = "Development environment for nixos-linux-as-bootloader";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      rust-overlay,
    }:
    let
      # Support multiple systems
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
    in
    {
      devShells = forAllSystems (
        system:
        let
          overlays = [ (import rust-overlay) ];
          pkgs = import nixpkgs {
            inherit system overlays;
          };

          rustToolchain = pkgs.rust-bin.stable.latest.default.override {
            extensions = [ "rust-src" "rust-analyzer" ];
          };
        in
        {
          default = pkgs.mkShell {
            packages = with pkgs; [
              # Nix language servers and formatters
              nil
              nixd
              nixfmt
              alejandra
              statix

              # General development tools
              git
              vim
              nano

              # Nix development and testing tools
              nix-tree
              nix-diff
              nix-output-monitor
              nixos-rebuild

              # Shell and scripting
              bash-language-server
              shellcheck
              shfmt

              # JSON/YAML tools (for flake.lock and configs)
              jq
              yq-go
              vscode-json-languageserver

              # Documentation and markdown
              marksman
              mdl

              # Version control helpers
              package-version-server

              # Rust toolchain and tools
              rustToolchain
              pkg-config
              openssl
              cargo-watch
              cargo-edit
              cargo-outdated
              clippy
              rustfmt

              # C/C++ build tools for Rust dependencies
              gcc
              gnumake
              cmake
            ];

            buildInputs = with pkgs; [
              openssl
            ];

            RUST_SRC_PATH = "${rustToolchain}/lib/rustlib/src/rust/library";
          };
        }
      );
    };
}
