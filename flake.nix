{
  description = "tend — workspace repository manager + fleet update controller";

  nixConfig = {
    allow-import-from-derivation = true;
  };

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";
    crate2nix.url = "github:nix-community/crate2nix";
    flake-utils.url = "github:numtide/flake-utils";
    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    substrate = {
      url = "github:pleme-io/substrate";
      inputs.nixpkgs.follows = "nixpkgs";
      inputs.fenix.follows = "fenix";
    };
    devenv = {
      url = "github:cachix/devenv";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = {
    self,
    nixpkgs,
    crate2nix,
    flake-utils,
    substrate,
    devenv,
    ...
  }: let
    # Existing CLI release outputs — preserves the GitHub Release
    # pipeline the rest of the fleet depends on (`nix run .#release`).
    releaseOutputs = (import "${substrate}/lib/rust-tool-release-flake.nix" {
      inherit nixpkgs crate2nix flake-utils devenv;
    }) {
      toolName = "pleme-tend";
      src = self;
      repo = "pleme-io/tend";

      # Configuration Management prime directive (★★, see
      # pleme-io/theory/CONFIGURATION-MANAGEMENT.md). Substrate's
      # rust-tool-release-flake.nix auto-generates the HM / NixOS /
      # Darwin module trio from this block. tend already uses
      # shikumi for its ~/.config/tend/config.yaml; this exports
      # the typed Nix surface so operators can declare workspaces
      # + daemon settings via home-manager and get the YAML
      # generated mechanically.
      module = {
        description = "tend — pleme-io workspace repository manager + K8s operator";
        hmNamespace = "blackmatter.components";

        # Shikumi YAML at ~/.config/tend/config.yaml. Operators
        # who want fully-declarative Nix-managed workspaces set
        # `programs.tend.settings.workspaces.* = …` and the YAML
        # is rendered. Operators who hand-author the YAML set
        # `manageConfig = false` (handled by substrate's macro)
        # and tend still picks up the file via shikumi discovery.
        withShikumiConfig = true;

        # Typed groups mirror what `tend status` / `tend sync`
        # read at startup. Free-form workspace declarations go
        # via `extraSettings.workspaces = { … }` from consumer
        # modules (the nix-private repo's tend wiring).
        shikumiTypedGroups = {
          daemon = {
            poll_interval_seconds = {
              type = "int";
              default = 300;
              description = "How often the tend daemon polls remote repo state (seconds).";
            };
            github_token_path = {
              type = "nullOrStr";
              default = null;
              description = "Path to a file containing the GitHub PAT used for org discovery. null = honor GITHUB_TOKEN env var.";
            };
          };

          # Throttle for upstream calls — surfaces samba's
          # quotaPct knob so operators can dial down GitHub
          # API pressure during a sync storm.
          throttle = {
            quota_pct = {
              type = "float";
              default = 0.01;
              description = "Fraction of upstream rate-limit to consume (samba pattern). 0.01 = 1%.";
            };
          };
        };
      };
    };

    # Docker image for the K8s operator. Same binary as the CLI release
    # — the `operator` Cargo feature is default so `tend operator` is
    # always present. Image targets x86_64-linux because rio (only
    # deployment target so far) is amd64.
    imageSystem = "x86_64-linux";
    pkgsLinux = import nixpkgs { system = imageSystem; };
    cargoNix = pkgsLinux.callPackage ./Cargo.nix {};
    tendBin = cargoNix.workspaceMembers."pleme-tend".build;

    # Minimal /etc/passwd + /etc/group + writable /home/tend so nix's
    # `getpwuid_r` doesn't abort with "cannot determine user's home
    # directory" before HOME is consulted. Without this layer the image
    # has no /etc/passwd at all and every nix invocation crashes — even
    # with HOME explicitly set on the subprocess, because some nix code
    # paths look up the user before falling back to HOME.
    fakeNss = pkgsLinux.runCommand "tend-fake-nss" { } ''
      mkdir -p $out/etc $out/home/tend $out/tmp
      cat > $out/etc/passwd <<'EOF'
      root:x:0:0:root:/root:/bin/sh
      tend:x:1000:1000:tend operator:/home/tend:/bin/sh
      nobody:x:65534:65534:nobody:/var/empty:/bin/sh
      EOF
      cat > $out/etc/group <<'EOF'
      root:x:0:
      tend:x:1000:
      nobody:x:65534:
      EOF
    '';

    dockerImage = pkgsLinux.dockerTools.buildLayeredImage {
      name = "ghcr.io/pleme-io/tend";
      tag = "amd64-${if (self ? rev) then builtins.substring 0 8 self.rev else "dev"}";
      contents = [
        tendBin
        # `tend operator`'s apply path shells out to git + nix.
        pkgsLinux.git
        pkgsLinux.openssh
        pkgsLinux.nix
        pkgsLinux.cacert
        # /tmp etc. needed for tokio::process spawn cwd resolution.
        pkgsLinux.coreutils
        fakeNss
      ];
      config = {
        Entrypoint = [ "${tendBin}/bin/tend" ];
        Cmd = [ "operator" ];
        Env = [
          "PATH=/bin"
          "SSL_CERT_FILE=${pkgsLinux.cacert}/etc/ssl/certs/ca-bundle.crt"
          "RUST_LOG=info,tend=debug,kube=info"
        ];
        WorkingDir = "/var/lib/tend-workspace";
      };
    };

    # GHCR push app — uses skopeo. Auth via ~/.docker/config.json or
    # GHCR_TOKEN env var (consumed by skopeo's --src-creds equivalent).
    pushImageApp = system: let
      pkgs = import nixpkgs { inherit system; };
    in {
      type = "app";
      program = toString (pkgs.writeShellScript "tend-push-image" ''
        set -euo pipefail
        IMAGE_PATH="$(nix build --no-link --print-out-paths .#dockerImage)"
        TAG="''${TAG:-amd64-latest}"
        echo "Pushing $IMAGE_PATH → docker://ghcr.io/pleme-io/tend:$TAG"
        ${pkgs.skopeo}/bin/skopeo copy \
          --insecure-policy \
          docker-archive:"$IMAGE_PATH" \
          docker://ghcr.io/pleme-io/tend:"$TAG"
      '');
    };

    imageOutputs = {
      packages.${imageSystem}.dockerImage = dockerImage;
      apps.${imageSystem}.push-image = pushImageApp imageSystem;
      apps."aarch64-darwin".push-image = pushImageApp "aarch64-darwin";
      apps."x86_64-darwin".push-image = pushImageApp "x86_64-darwin";
    };
  in
    nixpkgs.lib.recursiveUpdate releaseOutputs imageOutputs;
}
