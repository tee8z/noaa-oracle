{
  description = "NOAA Oracle - Decentralized weather data oracle for DLC-based prediction markets";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    crane.url = "github:ipetkov/crane";
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay, crane, ... }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs {
          inherit system overlays;
        };

        # Use stable Rust (1.85 required for duckdb 1.4 and arrow 56)
        rustToolchain = pkgs.rust-bin.stable."1.85.0".default.override {
          extensions = [ "rust-src" "rust-analyzer" ];
        };

        craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;

        # DuckDB version
        duckdbVersion = "1.0.0";

        # Download DuckDB library
        duckdb-lib = pkgs.stdenv.mkDerivation {
          pname = "duckdb-lib";
          version = duckdbVersion;

          src = pkgs.fetchurl {
            url = "https://github.com/duckdb/duckdb/releases/download/v${duckdbVersion}/libduckdb-linux-amd64.zip";
            sha256 = "sha256-m462XSq49lIQApYCEiGU17RnKKlFDMPVKABedMfjsgY=";
          };

          nativeBuildInputs = [ pkgs.unzip ];

          unpackPhase = ''
            unzip $src
          '';

          installPhase = ''
            mkdir -p $out/lib $out/include
            cp -r *.so* $out/lib/
            cp -r *.h $out/include/ 2>/dev/null || true
          '';
        };

        # System dependencies
        commonDeps = with pkgs; [
          pkg-config
          openssl
          openssl.dev
          perl
        ];

        # Build dependencies including DuckDB
        buildDeps = commonDeps ++ [ duckdb-lib ];

        # Environment for OpenSSL
        opensslEnv = {
          OPENSSL_NO_VENDOR = "1";
          OPENSSL_DIR = "${pkgs.openssl.dev}";
          OPENSSL_LIB_DIR = "${pkgs.openssl.out}/lib";
          OPENSSL_INCLUDE_DIR = "${pkgs.openssl.dev}/include";
        };

        # Source filtering
        src = pkgs.lib.cleanSourceWith {
          src = craneLib.path ./.;
          filter = path: type:
            (craneLib.filterCargoSources path type) ||
            (builtins.match ".*ui/.*" path != null) ||
            (builtins.match ".*config/.*" path != null) ||
            (builtins.match ".*\\.toml$" path != null);
        };

        # Common environment
        commonEnv = {
          DUCKDB_LIB_DIR = "${duckdb-lib}/lib";
          LD_LIBRARY_PATH = "${duckdb-lib}/lib";
        } // opensslEnv;

        # Build workspace dependencies once
        workspaceDeps = craneLib.buildDepsOnly ({
          pname = "noaa-oracle-workspace-deps";
          version = "0.1.0";
          inherit src;
          buildInputs = buildDeps;
          nativeBuildInputs = buildDeps;
        } // commonEnv);

        # Oracle server
        oracle = craneLib.buildPackage ({
          pname = "oracle";
          version = "1.9.2";
          inherit src;
          cargoArtifacts = workspaceDeps;
          buildInputs = buildDeps;
          nativeBuildInputs = buildDeps;
          cargoExtraArgs = "--bin oracle";

          postInstall = ''
            mkdir -p $out/share/noaa-oracle
            cp -r ui $out/share/noaa-oracle/
            cp -r config $out/share/noaa-oracle/
          '';
        } // commonEnv);

        # Daemon
        daemon = craneLib.buildPackage ({
          pname = "daemon";
          version = "1.9.2";
          inherit src;
          cargoArtifacts = workspaceDeps;
          buildInputs = commonDeps;
          nativeBuildInputs = commonDeps;
          cargoExtraArgs = "--bin daemon";
        } // commonEnv);

        # Development shell
        devShell = pkgs.mkShell {
          buildInputs = buildDeps ++ [
            rustToolchain
            pkgs.just
            pkgs.cargo-edit
            pkgs.lld
            pkgs.stdenv.cc.cc.lib  # Provides libstdc++ for DuckDB
          ];

          shellHook = ''
            export DUCKDB_LIB_DIR="${duckdb-lib}/lib"
            export LD_LIBRARY_PATH="${duckdb-lib}/lib:${pkgs.stdenv.cc.cc.lib}/lib:$LD_LIBRARY_PATH"
            export RUSTFLAGS="-C link-arg=-fuse-ld=lld"

            echo "NOAA Oracle Development Environment"
            echo "  Rust: ${rustToolchain.version}"
            echo "  DuckDB: ${duckdbVersion}"
            echo ""
            echo "Commands:"
            echo "  just build      - Build all crates"
            echo "  just run-oracle - Run oracle server"
            echo "  just run-daemon - Run data daemon"
            echo "  just check      - Run fmt, clippy, tests"
          '';

          DUCKDB_LIB_DIR = "${duckdb-lib}/lib";
        };

        # Runner scripts
        run-oracle = pkgs.writeShellScriptBin "noaa-oracle" ''
          export LD_LIBRARY_PATH="${duckdb-lib}/lib:$LD_LIBRARY_PATH"
          export NOAA_ORACLE_UI_DIR="''${NOAA_ORACLE_UI_DIR:-${oracle}/share/noaa-oracle/ui}"
          exec ${oracle}/bin/oracle "$@"
        '';

        run-daemon = pkgs.writeShellScriptBin "noaa-daemon" ''
          exec ${daemon}/bin/daemon "$@"
        '';

      in
      {
        packages = {
          inherit oracle daemon duckdb-lib;
          default = oracle;
        };

        apps = {
          oracle = flake-utils.lib.mkApp {
            drv = run-oracle;
            name = "noaa-oracle";
          };
          daemon = flake-utils.lib.mkApp {
            drv = run-daemon;
            name = "noaa-daemon";
          };
          default = flake-utils.lib.mkApp {
            drv = run-oracle;
            name = "noaa-oracle";
          };
        };

        devShells.default = devShell;

        checks = {
          inherit oracle daemon;

          clippy = craneLib.cargoClippy ({
            inherit src;
            cargoArtifacts = workspaceDeps;
            buildInputs = buildDeps;
            nativeBuildInputs = buildDeps;
            cargoClippyExtraArgs = "--all-targets -- -D warnings";
          } // commonEnv);

          fmt = craneLib.cargoFmt { inherit src; };
        };
      }
    ) // {
      # NixOS module
      nixosModules.default = { config, lib, pkgs, ... }:
        with lib;
        let
          cfg = config.services.noaa-oracle;
        in
        {
          options.services.noaa-oracle = {
            enable = mkEnableOption "NOAA Oracle weather data service";

            oracle = {
              enable = mkEnableOption "Oracle REST API server";

              package = mkOption {
                type = types.package;
                default = self.packages.${pkgs.system}.oracle;
                description = "Oracle package to use";
              };

              host = mkOption {
                type = types.str;
                default = "127.0.0.1";
                description = "Listen address";
              };

              port = mkOption {
                type = types.port;
                default = 9800;
                description = "Listen port";
              };

              dataDir = mkOption {
                type = types.path;
                default = "/var/lib/noaa-oracle";
                description = "Data directory for weather files and event database";
              };

              weatherDir = mkOption {
                type = types.str;
                default = "${cfg.oracle.dataDir}/weather";
                description = "Directory containing weather parquet files";
              };

              eventDb = mkOption {
                type = types.str;
                default = "${cfg.oracle.dataDir}/events";
                description = "Directory for event database";
              };
            };

            daemon = {
              enable = mkEnableOption "Data fetching daemon";

              package = mkOption {
                type = types.package;
                default = self.packages.${pkgs.system}.daemon;
                description = "Daemon package to use";
              };

              interval = mkOption {
                type = types.int;
                default = 3600;
                description = "Fetch interval in seconds";
              };

              oracleUrl = mkOption {
                type = types.str;
                default = "http://localhost:${toString cfg.oracle.port}";
                description = "Oracle server URL";
              };

              dataDir = mkOption {
                type = types.path;
                default = "/var/cache/noaa-oracle";
                description = "Cache directory for temporary parquet files";
              };
            };
          };

          config = mkIf cfg.enable {
            users.users.noaa-oracle = {
              isSystemUser = true;
              group = "noaa-oracle";
              home = cfg.oracle.dataDir;
              description = "NOAA Oracle service user";
            };

            users.groups.noaa-oracle = { };

            systemd.services.noaa-oracle = mkIf cfg.oracle.enable {
              description = "NOAA Oracle REST API Server";
              wantedBy = [ "multi-user.target" ];
              after = [ "network.target" ];

              environment = {
                NOAA_ORACLE_HOST = cfg.oracle.host;
                NOAA_ORACLE_PORT = toString cfg.oracle.port;
                NOAA_ORACLE_DATA_DIR = cfg.oracle.weatherDir;
                NOAA_ORACLE_EVENT_DB = cfg.oracle.eventDb;
                NOAA_ORACLE_UI_DIR = "${cfg.oracle.package}/share/noaa-oracle/ui";
                NOAA_ORACLE_PRIVATE_KEY = "${cfg.oracle.dataDir}/keys/oracle.pem";
                LD_LIBRARY_PATH = "${self.packages.${pkgs.system}.duckdb-lib}/lib";
                RUST_LOG = "info";
              };

              serviceConfig = {
                Type = "simple";
                User = "noaa-oracle";
                Group = "noaa-oracle";
                ExecStart = "${cfg.oracle.package}/bin/oracle";
                Restart = "always";
                RestartSec = 10;

                # Security
                NoNewPrivileges = true;
                ProtectSystem = "strict";
                ProtectHome = true;
                PrivateTmp = true;
                ReadWritePaths = [ cfg.oracle.dataDir ];
              };
            };

            systemd.services.noaa-daemon = mkIf cfg.daemon.enable {
              description = "NOAA Data Fetching Daemon";
              wantedBy = [ "multi-user.target" ];
              after = [ "network.target" ] ++ optional cfg.oracle.enable "noaa-oracle.service";
              requires = optional cfg.oracle.enable "noaa-oracle.service";

              environment = {
                NOAA_DAEMON_BASE_URL = cfg.daemon.oracleUrl;
                NOAA_DAEMON_DATA_DIR = cfg.daemon.dataDir;
                NOAA_DAEMON_SLEEP_INTERVAL = toString cfg.daemon.interval;
                RUST_LOG = "info";
              };

              serviceConfig = {
                Type = "simple";
                User = "noaa-oracle";
                Group = "noaa-oracle";
                ExecStart = "${cfg.daemon.package}/bin/daemon";
                Restart = "always";
                RestartSec = 60;

                # Security
                NoNewPrivileges = true;
                ProtectSystem = "strict";
                ProtectHome = true;
                PrivateTmp = true;
                ReadWritePaths = [ cfg.daemon.dataDir ];
              };
            };

            # Create directories
            systemd.tmpfiles.rules = [
              "d ${cfg.oracle.dataDir} 0750 noaa-oracle noaa-oracle -"
              "d ${cfg.oracle.dataDir}/weather 0750 noaa-oracle noaa-oracle -"
              "d ${cfg.oracle.dataDir}/events 0750 noaa-oracle noaa-oracle -"
              "d ${cfg.oracle.dataDir}/keys 0700 noaa-oracle noaa-oracle -"
              "d ${cfg.daemon.dataDir} 0750 noaa-oracle noaa-oracle -"
            ];
          };
        };
    };
}
