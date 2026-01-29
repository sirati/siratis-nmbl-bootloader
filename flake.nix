{
  description = "Development environment for nixos-linux-as-bootloader";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs =
    {
      self,
      nixpkgs,
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

      # Package definitions
      devPackages =
        pkgs: with pkgs; [
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
        ];
    in
    {
      devShells = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          default = pkgs.mkShell {
            packages = devPackages pkgs;

            shellHook = ''
              echo "╔════════════════════════════════════════════════════════════╗"
              echo "║     Nix development environment for nixos-linux-as-bootloader     ║"
              echo "╚════════════════════════════════════════════════════════════╝"
              echo ""
              echo "Nix version: $(nix --version)"
              echo ""
              echo "Available tools:"
              echo "  - nil, nixd: Nix language servers"
              echo "  - nixfmt-rfc-style, alejandra: Nix formatters"
              echo "  - statix: Nix linter"
              echo "  - nix-tree, nix-diff: Nix inspection tools"
              echo "  - shellcheck, shfmt: Shell script tools"
              echo ""
              echo "Ready to develop!"
            '';
          };
        }
      );
    };
}
