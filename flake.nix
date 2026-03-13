{
  description = "Nephara — AI World Simulation";

  inputs = {
    nixpkgs.url     = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay    = {
      url    = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, rust-overlay, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays     = [ (import rust-overlay) ];
        pkgs         = import nixpkgs { inherit system overlays; };
        rustToolchain = pkgs.rust-bin.stable.latest.default.override {
          extensions = [ "rust-src" "clippy" "rustfmt" ];
        };
      in {
        devShells.default = pkgs.mkShell {
          buildInputs = with pkgs; [
            rustToolchain
            pkg-config
            openssl
            python3Packages.huggingface-hub
          ];

          env = {
            RUST_LOG      = "info";
            RUST_SRC_PATH = "${rustToolchain}/lib/rustlib/src/rust/src";
          };

          shellHook = ''
            echo "=== Nephara Dev Shell ==="
            echo "  Fetch model:   huggingface-cli download <repo> <file> --local-dir models/"
            echo "  Mock run:      cargo run -- --llm mock"
            echo "  Live run:      cargo run"
            echo "  Seeded run:    cargo run -- --seed 42"
          '';
        };
      });
}
