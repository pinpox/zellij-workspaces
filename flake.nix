{
  description = "Zellij sidebar plugin: tabs grouped by project, with jj/git workspace management";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    { nixpkgs, rust-overlay, ... }:
    let
      forAllSystems =
        f:
        nixpkgs.lib.genAttrs [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ] (
          system:
          f (
            import nixpkgs {
              inherit system;
              overlays = [ rust-overlay.overlays.default ];
            }
          )
        );
      toolchain =
        pkgs: pkgs.rust-bin.stable.latest.default.override { targets = [ "wasm32-wasip1" ]; };
    in
    {
      packages = forAllSystems (
        pkgs:
        let
          rustPlatform = pkgs.makeRustPlatform {
            cargo = toolchain pkgs;
            rustc = toolchain pkgs;
          };
        in
        {
          default = rustPlatform.buildRustPackage {
            pname = "zellij-workspaces";
            version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).package.version;
            src = ./.;
            cargoLock = {
              lockFile = ./Cargo.lock;
              # zellij-tile comes from a pinned zellij git revision (see Cargo.toml)
              allowBuiltinFetchGit = true;
            };
            # zellij-tile's host-target deps (used by `cargo test`) link openssl
            nativeBuildInputs = [ pkgs.pkg-config ];
            buildInputs = [ pkgs.openssl ];
            # src/ws_sh_tests.rs drives real jj and git repositories
            nativeCheckInputs = [
              pkgs.jujutsu
              pkgs.git
            ];
            buildPhase = ''
              runHook preBuild
              cargo build --release --frozen --target wasm32-wasip1
              runHook postBuild
            '';
            # unit tests run on the host target
            checkPhase = ''
              runHook preCheck
              cargo test --frozen
              runHook postCheck
            '';
            installPhase = ''
              runHook preInstall
              install -Dm644 target/wasm32-wasip1/release/zellij-workspaces.wasm \
                $out/share/zellij/plugins/zellij-workspaces.wasm
              runHook postInstall
            '';
          };
        }
      );

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          packages = [
            (toolchain pkgs)
            pkgs.pkg-config
            pkgs.openssl
            pkgs.jujutsu
            pkgs.git
          ];
        };
      });
    };
}
