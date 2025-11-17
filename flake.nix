{
  description = "Portal healthcheck service flake";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      nixpkgs,
      rust-overlay,
      flake-utils,
      ...
    }:
    let
      inherit (nixpkgs) lib;

      mkPackage =
        pkgs:
        pkgs.rustPlatform.buildRustPackage {
          pname = "healthcheck-service";
          version = "0.1.0";
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;
          cargoHash = lib.fakeSha256;
          doCheck = false;

          nativeBuildInputs = with pkgs; [ pkg-config ];

          postInstall = ''
            shareDir=$out/share/healthcheck-service
            mkdir -p "$shareDir"

            if [ -d static ]; then
              mkdir -p "$shareDir/static"
              cp -r static/. "$shareDir/static/"
            fi

            if [ -d templates ]; then
              mkdir -p "$shareDir/templates"
              cp -r templates/. "$shareDir/templates/"
            fi
          '';
        };

      nixosModule =
        {
          config,
          pkgs,
          lib,
          ...
        }:
        let
          inherit (lib) mkEnableOption mkOption mkIf types optionalAttrs;

          cfg = config.services.healthcheck-service;
          packageDefault = mkPackage pkgs;
          shareDir = "${cfg.package}/share/healthcheck-service";

          envConfig =
            {
              RUST_LOG = cfg.rustLog;
              POLL_INTERVAL_SECS = toString cfg.pollIntervalSecs;
              REQUEST_TIMEOUT_MS = toString cfg.requestTimeoutMs;
              HEALTHCHECK_DB_PATH = cfg.healthcheckDbPath;
              OUTAGE_REMINDERS_ENABLED = if cfg.remindersEnabled then "true" else "false";
              OUTAGE_REMINDER_MINUTES = toString cfg.outageReminderMinutes;
              OUTAGE_REMINDER_REPEAT_MINUTES = toString cfg.outageReminderRepeatMinutes;
              OUTAGE_REMINDER_CHECK_SECS = toString cfg.outageReminderCheckSecs;
              ROCKET_ADDRESS = cfg.host;
              ROCKET_PORT = toString cfg.port;
              ROCKET_TEMPLATE_DIR = "${shareDir}/templates";
            }
            // optionalAttrs (cfg.discordWebhookUrl != null) {
              DISCORD_WEBHOOK_URL = cfg.discordWebhookUrl;
            };
        in
        {
          options.services.healthcheck-service = {
            enable = mkEnableOption "Portal's Rocket-based healthcheck service";

            package = mkOption {
              type = types.package;
              default = packageDefault;
              defaultText = "healthcheck-service package built from this flake";
              description = "Derivation that provides the healthcheck-service binary plus static assets.";
            };

            host = mkOption {
              type = types.str;
              default = "127.0.0.1";
              description = "Address Rocket should bind to.";
            };

            port = mkOption {
              type = types.port;
              default = 8000;
              description = "Port Rocket should listen on.";
            };

            user = mkOption {
              type = types.str;
              default = "healthcheck-service";
              description = "User that owns the systemd unit.";
            };

            group = mkOption {
              type = types.str;
              default = "healthcheck-service";
              description = "Group that owns the systemd unit.";
            };

            rustLog = mkOption {
              type = types.str;
              default = "info";
              description = "Value for RUST_LOG controlling tracing verbosity.";
            };

            pollIntervalSecs = mkOption {
              type = types.int;
              default = 60;
              description = "Polling cadence for downstream healthchecks.";
            };

            requestTimeoutMs = mkOption {
              type = types.int;
              default = 5000;
              description = "Default request timeout when a service does not override it.";
            };

            healthcheckDbPath = mkOption {
              type = types.str;
              default = "/var/lib/healthcheck-service/healthcheck.db";
              description = "SQLite database path used to persist incidents and snapshots.";
            };

            remindersEnabled = mkOption {
              type = types.bool;
              default = true;
              description = "Whether Discord outage reminders are active.";
            };

            outageReminderMinutes = mkOption {
              type = types.int;
              default = 30;
              description = "Minutes an outage must last before the first reminder fires.";
            };

            outageReminderRepeatMinutes = mkOption {
              type = types.int;
              default = 30;
              description = "Minutes between subsequent reminder notifications.";
            };

            outageReminderCheckSecs = mkOption {
              type = types.int;
              default = 60;
              description = "Interval for the reminder worker to scan for open incidents.";
            };

            discordWebhookUrl = mkOption {
              type = types.nullOr types.str;
              default = null;
              description = "Discord webhook URL for notifications (unset to disable).";
            };
          };

          config = mkIf cfg.enable {
            systemd.services.healthcheck-service = {
              description = "Portal Healthcheck Service";
              wantedBy = [ "multi-user.target" ];
              after = [ "network-online.target" ];
              wants = [ "network-online.target" ];

              environment = envConfig;

              serviceConfig = {
                ExecStart = "${cfg.package}/bin/healthcheck-service";
                WorkingDirectory = shareDir;
                Restart = "on-failure";
                RestartSec = 5;
                User = cfg.user;
                Group = cfg.group;
                StateDirectory = "healthcheck-service";
                ProtectSystem = "strict";
                ProtectHome = true;
                PrivateTmp = true;
                NoNewPrivileges = true;
              };
            };

            users.users.${cfg.user} = {
              isSystemUser = true;
              group = cfg.group;
            };

            users.groups.${cfg.group} = {};
          };
        };
    in
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs {
          inherit system overlays;
        };

        rustToolchain = pkgs.rust-bin.stable.latest.default.override {
          extensions = [ "rust-src" ];
        };

        healthcheckPackage = mkPackage pkgs;
      in
      {
        devShells.default = pkgs.mkShell {
          name = "healthcheck-service";
          buildInputs = [
            rustToolchain
            pkgs.rust-analyzer
            pkgs.pkg-config
            pkgs.sqlite
            pkgs.cargo-watch
          ];
        };

        packages.default = healthcheckPackage;

        checks.vm-test = pkgs.testers.runNixOSTest {
          name = "healthcheck-service-vm-test";

          nodes.machine = { config, pkgs, ... }: {
            imports = [ nixosModule ];

            networking.firewall.allowedTCPPorts = [ 8000 ];

            services.healthcheck-service = {
              enable = true;
              package = healthcheckPackage;
              host = "0.0.0.0";
              port = 8000;
              discordWebhookUrl = null;
              remindersEnabled = false;
              pollIntervalSecs = 5;
            };
          };

          testScript = ''
            machine.start()
            machine.wait_for_unit("healthcheck-service.service")
            machine.wait_until_succeeds("curl -f http://localhost:8000/api/status")
            print("✅ Healthcheck service is responding")
          '';
        };
      }
    )
    // {
      nixosModules.healthcheck-service = nixosModule;
    };
}
