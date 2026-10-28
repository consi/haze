{
  description = "Haze - network latency monitor with embedded UI";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      inherit (nixpkgs) lib;

      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];
      forAllSystems = f: lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});

      # Workspace manifests carry `0.0.0-dev`; release CI rewrites it from the
      # tag. Do the same here so `haze --version` reports something useful.
      baseVersion = "0.6.1";
      version = "${baseVersion}+${self.shortRev or self.dirtyShortRev or "dirty"}";
      devVersion = ''version = "0.0.0-dev"'';
      setVersion = builtins.replaceStrings [ devVersion ] [ ''version = "${version}"'' ];

      mkPackages =
        pkgs:
        let
          frontend = pkgs.buildNpmPackage {
            pname = "haze-frontend";
            # Stable version keeps the npm-deps FOD from refetching per commit.
            version = baseVersion;
            src = ./frontend;
            nodejs = pkgs.nodejs_24;
            npmDepsHash = "sha256-6RL7eZrZwBaFzgMC3X/11n+Az++TSj1C2KleIdwzFOU=";
            installPhase = ''
              runHook preInstall
              cp -r build $out
              runHook postInstall
            '';
          };

          # Only what cargo needs, so frontend/docs edits don't rebuild Rust.
          rustSrc = lib.fileset.toSource {
            root = ./.;
            fileset = lib.fileset.unions [
              ./Cargo.toml
              ./Cargo.lock
              ./.cargo
              ./crates
              ./migrations
              ./testdata
              ./packaging
            ];
          };

          # Patch the version in the source itself (not postPatch) so the
          # cargo-setup lockfile diff sees identical Cargo.lock files.
          src = pkgs.runCommand "haze-src-${version}" { } ''
            cp -r ${rustSrc} $out
            chmod -R u+w $out
            substituteInPlace $out/Cargo.toml $out/Cargo.lock \
              --replace-fail '${devVersion}' 'version = "${version}"'
          '';

          haze = pkgs.rustPlatform.buildRustPackage {
            pname = "haze";
            inherit version src;

            cargoLock.lockFileContents = setVersion (builtins.readFile ./Cargo.lock);

            postPatch = ''
              rm -rf frontend/build
              mkdir -p frontend
              cp -r ${frontend} frontend/build
              chmod -R u+w frontend/build
            '';

            cargoBuildFlags = [
              "-p"
              "haze-cli"
            ];

            nativeBuildInputs = [ pkgs.pkg-config ];
            buildInputs = [ pkgs.openssl ];
            # haze-cli forces openssl-sys/vendored for static musl releases;
            # link against nixpkgs' OpenSSL instead.
            env.OPENSSL_NO_VENDOR = 1;

            # Probe tests need network access and raw sockets; CI covers them.
            doCheck = false;

            postInstall = ''
              install -Dm644 packaging/haze.env.example $out/share/haze/haze.env.example
            '';

            passthru = { inherit frontend; };

            meta = {
              description = "Network latency monitor with smoke-style UI and embedded frontend";
              homepage = "https://github.com/consi/haze";
              license = lib.licenses.agpl3Plus;
              mainProgram = "haze";
              platforms = lib.platforms.linux ++ lib.platforms.darwin;
            };
          };
        in
        {
          inherit haze;
          haze-frontend = frontend;
          default = haze;
        };
    in
    {
      packages = forAllSystems mkPackages;

      apps = forAllSystems (pkgs: {
        default = {
          type = "app";
          program = lib.getExe self.packages.${pkgs.stdenv.hostPlatform.system}.haze;
        };
      });

      overlays.default = final: _prev: { haze = (mkPackages final).haze; };

      nixosModules = {
        haze =
          { pkgs, ... }:
          {
            imports = [ ./nix/module.nix ];
            services.haze.package = lib.mkDefault self.packages.${pkgs.stdenv.hostPlatform.system}.haze;
          };
        default = self.nixosModules.haze;
      };

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          inputsFrom = [ self.packages.${pkgs.stdenv.hostPlatform.system}.haze ];
          packages = with pkgs; [
            cargo
            rustc
            clippy
            rustfmt
            rust-analyzer
            nodejs_24
            just
            cargo-nextest
            cargo-deny
            cargo-audit
            cargo-zigbuild
            git-cliff
            zig
          ];
          env.OPENSSL_NO_VENDOR = 1;
        };
      });

      checks = lib.genAttrs [ "x86_64-linux" "aarch64-linux" ] (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          inherit (self.packages.${system}) haze;
          module = pkgs.testers.runNixOSTest {
            name = "haze-module";
            nodes.machine = {
              imports = [ self.nixosModules.default ];
              services.haze.enable = true;
            };
            testScript = ''
              machine.wait_for_unit("haze.service")
              machine.wait_for_open_port(4420)
              machine.succeed("curl -fsS http://127.0.0.1:4420/healthz")
              machine.fail("curl -fsS http://127.0.0.1:4420/ | grep -q Placeholder")
            '';
          };
        }
      );

      formatter = forAllSystems (pkgs: pkgs.nixfmt);
    };
}
