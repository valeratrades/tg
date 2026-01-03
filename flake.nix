{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
    flake-utils.url = "github:numtide/flake-utils";
    pre-commit-hooks.url = "github:cachix/git-hooks.nix/ca5b894d3e3e151ffc1db040b6ce4dcc75d31c37";
    v-utils.url = "github:valeratrades/.github/v1.2.1";
  };

  outputs =
    { self, nixpkgs, rust-overlay, flake-utils, pre-commit-hooks, v-utils }:
    let
      manifest = (nixpkgs.lib.importTOML ./Cargo.toml).package;
      pname = manifest.name;
    in
    flake-utils.lib.eachDefaultSystem
      (
        system:
        let
          overlays = builtins.trace "flake.nix sourced" [ (import rust-overlay) ];
          pkgs = import nixpkgs {
            inherit system overlays;
          };
          rust = pkgs.rust-bin.nightly."2025-10-10".default;
          pre-commit-check = pre-commit-hooks.lib.${system}.run (v-utils.files.preCommit { inherit pkgs; });
          stdenv = pkgs.stdenvAdapters.useMoldLinker pkgs.stdenv;

          github = v-utils.github {
            inherit pkgs pname;
            langs = [ "rs" ];
            lastSupportedVersion = "nightly-2025-10-10";
            jobsErrors = [ "rust-tests" ];
            jobsWarnings = [
              "rust-doc"
              "rust-clippy"
              "rust-machete"
              "rust-sorted"
              "rust-sorted-derives"
              "tokei"
            ];
            jobsOther = [ "loc-badge" ];
          };
          readme = v-utils.readme-fw {
            inherit pkgs pname;
            lastSupportedVersion = "nightly-1.92";
            rootDir = ./.;
            licenses = [{ license = v-utils.files.licenses.nsfw; }];
            badges = [ "msrv" "crates_io" "docs_rs" "loc" "ci" ];
          };
        in
        {
          packages =
            let
              rustc = rust;
              cargo = rust;
              rustPlatform = pkgs.makeRustPlatform {
                inherit rustc cargo stdenv;
              };
            in
            {
              default = rustPlatform.buildRustPackage rec {
                inherit pname;
                version = manifest.version;

                buildInputs = with pkgs; [
                  openssl.dev
                ];
                nativeBuildInputs = with pkgs; [ pkg-config ];

                cargoLock.lockFile = ./Cargo.lock;
                src = pkgs.lib.cleanSource ./.;
              };
            };

          devShells.default =
            with pkgs;
            mkShell {
              inherit stdenv;
              shellHook =
                pre-commit-check.shellHook +
                github.shellHook +
                readme.shellHook +
                ''
                  cp -f ${(v-utils.files.treefmt) { inherit pkgs; }} ./.treefmt.toml

                  mkdir -p ./.cargo
                  cp -f ${(v-utils.files.rust.config { inherit pkgs; })} ./.cargo/config.toml
                  cp -f ${(v-utils.files.rust.rustfmt { inherit pkgs; })} ./rustfmt.toml
                  cp -f ${(v-utils.files.rust.toolchain { inherit pkgs; })} ./.cargo/rust-toolchain.toml
                '';
              env = {
                RUST_BACKTRACE = 1;
                RUST_LIB_BACKTRACE = 0;
              };

              packages = [
                mold
                openssl
                pkg-config
                rust
              ]
              ++ pre-commit-check.enabledPackages
              ++ github.enabledPackages;
            };
        }
      )
    // {
      #good ref: https://github.com/NixOS/nixpkgs/blob/04ef94c4c1582fd485bbfdb8c4a8ba250e359195/nixos/modules/services/audio/navidrome.nix#L89
      homeManagerModules."${pname}" = { config, lib, pkgs, ... }:
        let
          inherit (lib) mkEnableOption mkOption mkIf;
          inherit (lib.types) package path nullOr;
          cfg = config."${pname}";
        in
        {
          options."${pname}" = {
            enable = mkEnableOption "";

            package = mkOption {
              type = package;
              default = self.packages.${pkgs.system}.default;
              description = "The package to use.";
            };

            token = mkOption {
              type = path;
              description = "Path to the file containing the Telegram token for LoadCredential.";
              example = "config.sops.secrets.telegram_token_main.path";
            };

            apiHash = mkOption {
              type = nullOr path;
              default = null;
              description = "Path to the file containing the Telegram API hash for MTProto.";
              example = "config.sops.secrets.telegram_api_hash.path";
            };

            phone = mkOption {
              type = nullOr path;
              default = null;
              description = "Path to the file containing the phone number for MTProto.";
              example = "config.sops.secrets.telegram_phone.path";
            };
          };

          config = mkIf cfg.enable {
            systemd.user.services.tg-server = {
              Unit = {
                Description = "tg server";
                After = [ "network.target" ];
              };

              Install = {
                WantedBy = [ "default.target" ];
              };

              Service = {
                Type = "simple";
                LoadCredential = [
                  "tg_token:${cfg.token}"
                ] ++ lib.optional (cfg.apiHash != null) "tg_api_hash:${cfg.apiHash}"
                ++ lib.optional (cfg.phone != null) "tg_phone:${cfg.phone}";
                ExecStart =
                  let
                    envSetup = lib.concatStringsSep " " (
                      lib.optional (cfg.apiHash != null) ''TELEGRAM_API_HASH="$(cat %d/tg_api_hash)"''
                      ++ lib.optional (cfg.phone != null) ''PHONE_NUMBER_FR="$(cat %d/tg_phone)"''
                    );
                  in
                  ''
                    /bin/sh -c '${envSetup} ${cfg.package}/bin/${pname} --token "$(cat %d/tg_token)" server'
                  '';
                Restart = "on-failure";
              };
            };

            home.packages = [ cfg.package ];
          };
        };
    };
}
