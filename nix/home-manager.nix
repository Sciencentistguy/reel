self:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.programs.vlib;
  shared = import ./options.nix self { inherit lib pkgs; };
in
{
  options.programs.vlib = shared.options;

  config = lib.mkIf cfg.enable {
    home.packages = [ cfg.package ];
    xdg.configFile."vlib/config.toml".source = shared.toml.generate "vlib-config.toml" cfg.settings;
  };
}
