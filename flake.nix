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

          commonArgs = {
            inherit src;
            strictDeps = true;
            pname = "imap-mcp-rs";
            version = "0.1.0";
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

          # Clippy
          clippy = s.craneLib.cargoClippy (s.commonArgs // {
            inherit (s) cargoArtifacts;
            cargoClippyExtraArgs = "-- -W clippy::all -W clippy::pedantic -A clippy::module_name_repetitions -A clippy::must_use_candidate -A clippy::missing_errors_doc -A clippy::missing_panics_doc -A clippy::doc_markdown";
          });

          # Formatting
          fmt = s.craneLib.cargoFmt s.commonArgs;
        }
      );

      apps = forAllSystems (system: {
        default = {
          type = "app";
          program = "${self.packages.${system}.default}/bin/imap-mcp-rs";
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
