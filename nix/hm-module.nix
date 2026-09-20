{ self }:

{ config, lib, pkgs, ... }:

let
  cfg = config.services.ledecky;

  inherit (lib) mkIf mkOption mkEnableOption types;

  toValue = v: if lib.isBool v then lib.boolToString v else toString v;

  # `watch_debounce = 250` becomes `LEDECKY_WATCH_DEBOUNCE=250`. Every key in
  # `Settings` is reachable this way; the named options below are the ones worth
  # documenting.
  settingsEnv =
    lib.mapAttrsToList (k: v: "LEDECKY_${lib.toUpper k}=${toValue v}") cfg.settings;
in
{
  options.services.ledecky = {
    enable = mkEnableOption "ledecky, a local kanban board for Claude Code agents";

    package = mkOption {
      type = types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      defaultText = lib.literalMD "the `ledecky` flake's own package";
      description = "The ledecky package to run.";
    };

    address = mkOption {
      type = types.str;
      default = "127.0.0.1";
      description = ''
        Address to bind. Agents reach their hooks over the loopback address
        whatever this says, so widening it only widens who can see the board.
      '';
    };

    port = mkOption {
      type = types.port;
      default = 8770;
      description = ''
        Port to listen on, or 0 to take a free one. Hook URLs are built from the
        port actually bound, so agents follow it either way — but a fixed port is
        what makes a `allowedHttpHookUrls` allowlist possible.
      '';
    };

    settings = mkOption {
      type = types.attrsOf (types.oneOf [ types.str types.int types.bool ]);
      default = { };
      example = { agent_bin = "claude"; watch_debounce = 250; };
      description = ''
        Any other key from `Rocket.toml`, passed through as a `LEDECKY_*`
        environment variable. See the Configuration section of the README.
      '';
    };
  };

  config = mkIf cfg.enable {
    systemd.user.services.ledecky = {
      Unit = {
        Description = "ledecky — Claude Code kanban agent manager";
        Documentation = "https://github.com/chrisseto/ledecky";
        After = [ "graphical-session.target" ];
      };

      Service = {
        ExecStart = lib.getExe cfg.package;

        # NB: no PATH. A user unit inherits the session's, which carries the
        # profile directories an agent is installed into, and `Environment=`
        # replaces rather than prepends — setting it here would take that away.
        # `git` and `delta` come from the package's wrapper, which prefixes
        # them so its own pinned pair wins.
        Environment = [
          "ROCKET_ADDRESS=${cfg.address}"
          "ROCKET_PORT=${toString cfg.port}"
        ] ++ settingsEnv;

        Restart = "on-failure";
        RestartSec = 5;

        # Agents are children of this process and are killed by its own shutdown
        # hook; taking the cgroup down with the main process would pre-empt that
        # and strand their worktrees.
        KillMode = "mixed";
        TimeoutStopSec = 30;
      };

      Install.WantedBy = [ "default.target" ];
    };
  };
}
