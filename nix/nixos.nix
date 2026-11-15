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
    environment.systemPackages = [ cfg.package ];
    # Read by any user without a configuration file of their own.
    environment.etc."vlib/config.toml".source = shared.toml.generate "vlib-config.toml" cfg.settings;
  };
}
