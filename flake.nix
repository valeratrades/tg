{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
    flake-utils.url = "github:numtide/flake-utils";
    pre-commit-hooks.url = "github:cachix/git-hooks.nix";
    v-utils.url = "github:valeratrades/.github";
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

          workflowContents = v-utils.ci {
            inherit pkgs;
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
            licenses = [
              {
                name = "Blue Oak 1.0.0";
                outPath = "LICENSE";
              }
            ];
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
              shellHook = pre-commit-check.shellHook + ''
                            mkdir -p ./.github/workflows
                            cp ${workflowContents.errors} -f ./.github/workflows/errors.yml
                            cp ${workflowContents.warnings} -f ./.github/workflows/warnings.yml
                            cp ${workflowContents.other} -f ./.github/workflows/other.yml || :

                             if command -v gh &> /dev/null; then
                  if [ -n "$GITHUB_LOC_GIST" ]; then
                    echo "Setting GITHUB_LOC_GIST secret for repository..."
                    gh secret set GITHUB_LOC_GIST --body "$GITHUB_LOC_GIST" 2>/dev/null || echo "Failed to set secret (may already exist or no permissions)"
                  else
                    echo "Warning: GITHUB_LOC_GIST environment variable not set. The LOC badge workflow will fail."
                  fi
                fi


                            cp -f ${v-utils.files.licenses.blue_oak} ./LICENSE

                            ${v-utils.hooks.appendCustom} ./.git/hooks/pre-commit
                            cp -f ${(v-utils.hooks.treefmt) { inherit pkgs; }} ./.treefmt.toml
                            cp -f ${(v-utils.hooks.preCommit) { inherit pkgs pname; }} ./.git/hooks/custom.sh

                            mkdir -p ./.cargo
                            cp -f ${
                              (v-utils.files.gitignore {
                                inherit pkgs;
                                langs = [ "rs" ];
                              })
                            } ./.gitignore
                            cp -f ${(v-utils.files.rust.config { inherit pkgs; })} ./.cargo/config.toml
                            cp -f ${(v-utils.files.rust.rustfmt { inherit pkgs; })} ./rustfmt.toml
                            cp -f ${(v-utils.files.rust.toolchain { inherit pkgs; })} ./.cargo/rust-toolchain.toml

                            cp -f ${readme} ./README.md
              '';
              env = {
                RUST_BACKTRACE = 1;
                RUST_LIB_BACKTRACE = 0;
              };

              packages = [
                mold-wrapped
                openssl
                pkg-config
                rust
              ]
              ++ pre-commit-check.enabledPackages;
            };
        }
      )
    // {
      #good ref: https://github.com/NixOS/nixpkgs/blob/04ef94c4c1582fd485bbfdb8c4a8ba250e359195/nixos/modules/services/audio/navidrome.nix#L89
      homeManagerModules."${pname}" = { config, lib, pkgs, ... }:
        let
          inherit (lib) mkEnableOption mkOption mkIf;
          inherit (lib.types) package path;
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
                LoadCredential = "tg_token:${cfg.token}";
                ExecStart = ''
                  /bin/sh -c '${cfg.package}/bin/${pname} --token "$(cat %d/tg_token)" server'
                '';
                Restart = "on-failure";
              };
            };

            home.packages = [ cfg.package ];
          };
        };
    };
}
