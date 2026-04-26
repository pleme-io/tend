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
    };

    # Docker image for the K8s operator. Same binary as the CLI release
    # — the `operator` Cargo feature is default so `tend operator` is
    # always present. Image targets x86_64-linux because rio (only
    # deployment target so far) is amd64.
    imageSystem = "x86_64-linux";
    pkgsLinux = import nixpkgs { system = imageSystem; };
    cargoNix = pkgsLinux.callPackage ./Cargo.nix {};
    tendBin = cargoNix.workspaceMembers."pleme-tend".build;

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
