{
  description = "imap-mcp-rs — IMAP MCP Server in Rust";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    crane.url = "github:ipetkov/crane";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, crane, rust-overlay, ... }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];
      forAllSystems = nixpkgs.lib.genAttrs systems;

      # Shared per-system setup — evaluated once, used by packages + devShells + checks
      perSystem = system:
        let
          pkgs = import nixpkgs {
            inherit system;
            overlays = [ rust-overlay.overlays.default ];
          };
          toolchain = pkgs.rust-bin.stable.latest.default;
          craneLib = (crane.mkLib pkgs).overrideToolchain toolchain;
          src = craneLib.cleanCargoSource ./.;

          # Name and version come from Cargo.toml so a release only ever has
          # to touch one file — a hardcoded copy here silently disagrees with
          # `--version` the first time someone forgets it.
          cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);

          commonArgs = {
            inherit src;
            strictDeps = true;
            pname = cargoToml.package.name;
            version = cargoToml.package.version;
          };

          cargoArtifacts = craneLib.buildDepsOnly commonArgs;
        in
        {
          inherit pkgs toolchain craneLib commonArgs cargoArtifacts;
        };
    in
    {
      packages = forAllSystems (system:
        let
          s = perSystem system;
          pkg = s.craneLib.buildPackage (s.commonArgs // {
            inherit (s) cargoArtifacts;
            meta = {
              description = "IMAP MCP Server — email tools for LLM assistants";
              mainProgram = "imap-mcp-rs";
            };
          });
        in
        {
          default = pkg;
          imap-mcp-rs = pkg;
        }
      );

      checks = forAllSystems (system:
        let
          s = perSystem system;
        in
        {
          # Build
          package = self.packages.${system}.default;

          # Clippy — exactly the CI gate (all targets, pedantic + nursery,
          # warnings denied), so a green `nix flake check` means a green CI
          # lint job. Overriding the args replaces crane's own
          # `--all-targets` default, hence it is spelled out here.
          clippy = s.craneLib.cargoClippy (s.commonArgs // {
            inherit (s) cargoArtifacts;
            cargoClippyExtraArgs = "--all-targets -- -D warnings -W clippy::pedantic -W clippy::nursery";
          });

          # Formatting
          fmt = s.craneLib.cargoFmt s.commonArgs;
        }
      );

      apps = forAllSystems (system: {
        default = {
          type = "app";
          program = "${self.packages.${system}.default}/bin/imap-mcp-rs";
          meta.description = "IMAP MCP Server — email tools for LLM assistants";
        };
      });

      devShells = forAllSystems (system:
        let
          s = perSystem system;
          devToolchain = s.toolchain.override {
            extensions = [ "rust-src" "rust-analyzer" "clippy" ];
          };
        in
        {
          default = s.pkgs.mkShell {
            nativeBuildInputs = [
              devToolchain
            ];
          };
        }
      );
    };
}
