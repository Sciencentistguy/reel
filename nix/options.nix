# Options shared by the NixOS and home-manager modules.
self:
{ lib, pkgs }:
let
  toml = pkgs.formats.toml { };
in
{
  inherit toml;

  options = {
    enable = lib.mkEnableOption "reel, a library manager for films and TV shows";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.reel;
      defaultText = lib.literalExpression "reel.packages.\${system}.reel";
      description = "The reel package to install.";
    };

    settings = lib.mkOption {
      inherit (toml) type;
      default = { };
      example = lib.literalExpression ''
        {
          library = {
            movies = "/srv/media/Movies";
            tv = "/srv/media/TV Shows";
          };
          import.operation = "hardlink";
          metadata = {
            tv_source = "tvdb";
            tmdb_api_key_file = "/run/secrets/tmdb";
            tvdb_api_key_file = "/run/secrets/tvdb";
          };
        }
      '';
      description = ''
        Contents of reel's `config.toml`; anything left out keeps its default.

        The file ends up world-readable in the Nix store, so supply API keys through
        `metadata.tmdb_api_key_file`, `metadata.tvdb_api_key_file` and
        `metadata.thesportsdb_api_key_file` rather than inline.
      '';
    };
  };
}
