{ self }:

{ config, lib, pkgs, ... }:

let
  cfg = config.services.ledecky;

  inherit (lib) mkIf mkOption mkEnableOption types;

  toml = pkgs.formats.toml { };

  # Named in the unit, so a change to it is a change to the unit and restarts
  # the service.
  configFile = toml.generate "ledecky.toml" {
    default = cfg.settings // {
      inherit (cfg) address port;
      hook_address = cfg.hookAddress;
      hook_port = cfg.hookPort;
    };
  };
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
        Address the board binds. Hooks are served on loopback whatever this
        says, so widening it only widens who can see the board.
      '';
    };

    port = mkOption {
      type = types.port;
      default = 8770;
      description = ''
        The board's port, or 0 to take a free one.
      '';
    };

    hookAddress = mkOption {
      type = types.str;
      default = "127.0.0.1";
      description = ''
        Address hooks are served on. Agents are handed URLs naming it, so
        widening it hands those URLs out wider too.
      '';
    };

    hookPort = mkOption {
      type = types.port;
      default = 8771;
      description = ''
        Loopback port hooks are served on, or 0 to take a free one. Hook URLs are
        built from the port actually bound, so agents follow it either way — but
        a fixed port is what makes a `allowedHttpHookUrls` allowlist possible.
      '';
    };

    settings = mkOption {
      type = toml.type;
      default = { };
      example = { agent_bin = "claude"; watch_debounce = 250; };
      description = ''
        Any other key from `ledecky.toml`, written under `[default]` in the
        config file the service is started with. See the Configuration section
        of the README.
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
        ExecStart = "${lib.getExe cfg.package} --config ${configFile}";

        # NB: no `Environment=` PATH. A user unit inherits the session's, which
        # carries the profile directories an agent is installed into, and
        # `Environment=` would replace it rather than prepend.

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
