{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.haze;
  defaultDataDir = "/var/lib/haze";
in
{
  options.services.haze = {
    enable = lib.mkEnableOption "Haze network latency monitor";

    package = lib.mkPackageOption pkgs "haze" { };

    user = lib.mkOption {
      type = lib.types.str;
      default = "haze";
      description = "User that runs Haze. A group of the same name is created as well.";
    };

    group = lib.mkOption {
      type = lib.types.str;
      default = "haze";
      description = "Group that runs Haze.";
    };

    host = lib.mkOption {
      type = lib.types.str;
      default = "127.0.0.1";
      example = "0.0.0.0";
      description = "Address the web UI and API bind to.";
    };

    port = lib.mkOption {
      type = lib.types.port;
      default = 4420;
      description = "Port the web UI and API listen on.";
    };

    origin = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "https://haze.example.com";
      description = ''
        Public origin (scheme, host, optional port; no path) the browser uses
        to reach Haze. Required for WebAuthn passkeys; password login works
        without it. Must exactly match the browser address bar.
      '';
    };

    baseUrl = lib.mkOption {
      type = lib.types.str;
      default = "";
      example = "/haze";
      description = "URL path prefix when served behind a reverse proxy under a sub-path. Empty serves at `/`.";
    };

    dataDir = lib.mkOption {
      type = lib.types.path;
      default = defaultDataDir;
      description = "Directory holding the SQLite database and probe chunk files.";
    };

    logLevel = lib.mkOption {
      type = lib.types.str;
      default = "haze=info";
      example = "haze=debug,info";
      description = "`tracing-subscriber` filter directive (`HAZE_LOG`).";
    };

    environmentFile = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = null;
      example = "/run/secrets/haze.env";
      description = "Extra `HAZE_*` environment variables loaded from a file, for values that should stay out of the Nix store.";
    };

    extraEnvironment = lib.mkOption {
      type = lib.types.attrsOf lib.types.str;
      default = { };
      description = "Additional environment variables for the Haze process.";
    };

    openFirewall = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = "Open {option}`services.haze.port` in the firewall.";
    };
  };

  config = lib.mkIf cfg.enable {
    users.users = lib.mkIf (cfg.user == "haze") {
      haze = {
        isSystemUser = true;
        group = cfg.group;
        home = cfg.dataDir;
        description = "Haze network latency monitor";
      };
    };
    users.groups = lib.mkIf (cfg.group == "haze") { haze = { }; };

    systemd.tmpfiles.settings.haze = lib.mkIf (cfg.dataDir != defaultDataDir) {
      ${cfg.dataDir}.d = {
        inherit (cfg) user group;
        mode = "0750";
      };
    };

    networking.firewall.allowedTCPPorts = lib.mkIf cfg.openFirewall [ cfg.port ];

    systemd.services.haze = {
      description = "Haze network latency monitor";
      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];
      wantedBy = [ "multi-user.target" ];

      environment = {
        HAZE_BIND = "${cfg.host}:${toString cfg.port}";
        HAZE_DATA_DIR = cfg.dataDir;
        HAZE_LOG = cfg.logLevel;
        HAZE_BASE_URL = cfg.baseUrl;
      }
      // lib.optionalAttrs (cfg.origin != null) { HAZE_ORIGIN = cfg.origin; }
      // cfg.extraEnvironment;

      serviceConfig = {
        Type = "simple";
        ExecStart = lib.getExe cfg.package;
        Restart = "on-failure";
        RestartSec = "5s";
        User = cfg.user;
        Group = cfg.group;
        WorkingDirectory = cfg.dataDir;
        StateDirectory = lib.mkIf (cfg.dataDir == defaultDataDir) "haze";
        StateDirectoryMode = "0750";
        EnvironmentFile = lib.mkIf (cfg.environmentFile != null) cfg.environmentFile;
        ReadWritePaths = lib.mkIf (cfg.dataDir != defaultDataDir) [ cfg.dataDir ];

        # Each monitored host keeps two FDs open (lock file + active WAL), and
        # @resources is filtered below, so the process can't raise this itself.
        LimitNOFILE = 65536;

        # Minimum capability for ICMP probes.
        AmbientCapabilities = [ "CAP_NET_RAW" ];
        CapabilityBoundingSet = [ "CAP_NET_RAW" ];
        NoNewPrivileges = true;

        # Sandboxing (mirrors packaging/systemd/haze.service).
        ProtectSystem = "strict";
        ProtectHome = true;
        PrivateTmp = true;
        PrivateDevices = true;
        ProtectKernelTunables = true;
        ProtectKernelModules = true;
        ProtectControlGroups = true;
        ProtectClock = true;
        ProtectHostname = true;
        ProtectKernelLogs = true;
        RestrictNamespaces = true;
        RestrictRealtime = true;
        RestrictSUIDSGID = true;
        LockPersonality = true;
        MemoryDenyWriteExecute = true;
        SystemCallArchitectures = "native";
        SystemCallFilter = [
          "@system-service"
          "~@privileged @resources"
        ];
      };
    };
  };
}
