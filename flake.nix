{
  description = "Library manager for films and TV shows using Plex naming conventions";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      inherit (nixpkgs) lib;
      forAllSystems =
        f: lib.genAttrs lib.systems.flakeExposed (system: f nixpkgs.legacyPackages.${system});
      manifest = (lib.importTOML ./Cargo.toml).package;
    in
    {
      packages = forAllSystems (pkgs: {
        default = self.packages.${pkgs.stdenv.hostPlatform.system}.reel;

        reel = pkgs.rustPlatform.buildRustPackage {
          pname = manifest.name;
          inherit (manifest) version;

          src = lib.fileset.toSource {
            root = ./.;
            fileset = lib.fileset.unions [
              ./Cargo.toml
              ./Cargo.lock
              ./src
            ];
          };
          cargoLock.lockFile = ./Cargo.lock;

          nativeBuildInputs = [
            pkgs.makeBinaryWrapper
            pkgs.pkg-config
          ];
          # nixpkgs links libsqlite3-sys against the system SQLite rather than the bundled copy.
          buildInputs = [ pkgs.sqlite ];

          # ffprobe supplies the media information recorded on import.
          postInstall = ''
            wrapProgram $out/bin/reel \
              --suffix PATH : ${lib.makeBinPath [ pkgs.ffmpeg-headless ]}
          '';

          meta = {
            inherit (manifest) description;
            license = lib.licenses.gpl3;
            mainProgram = "reel";
          };
        };
      });

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          inputsFrom = [ self.packages.${pkgs.stdenv.hostPlatform.system}.reel ];
          packages = [
            pkgs.clippy
            pkgs.rustfmt
            pkgs.rust-analyzer
            pkgs.ffmpeg-headless
          ];
        };
      });

      formatter = forAllSystems (pkgs: pkgs.nixfmt);
    };
}
