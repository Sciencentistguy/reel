self:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.programs.reel;
  shared = import ./options.nix self { inherit lib pkgs; };
in
{
  options.programs.reel = shared.options;

  config = lib.mkIf cfg.enable {
    environment.systemPackages = [ cfg.package ];
    # Read by any user without a configuration file of their own.
    environment.etc."reel/config.toml".source = shared.toml.generate "reel-config.toml" cfg.settings;
  };
}
