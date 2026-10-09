{
  description = "tend — workspace repository manager + fleet update controller";

  # substrate.rust.tool dispatches over Cargo.gen.lock (the slim gen delta,
  # reconstructed to the full BuildSpec in pure Nix) — no crate2nix, no Cargo.nix.
  inputs.substrate.url = "github:pleme-io/substrate";

  outputs =
    { substrate, ... }:
    let
      tool = substrate.rust.tool { src = ./.; };
    in
    tool
    // {
      lib = (tool.lib or { }) // {
        # tend's config, as JSON Schema (draft 2020-12) — the committed,
        # golden-tested output of `src/schema.rs` (`tend config-schema`).
        # Consumers generate typed options from it instead of restating
        # them, e.g. the blackmatter-tend home-manager module:
        #   substrate.lib.types.jsonSchema.optionsFromJsonSchema {
        #     inherit lib; schema = tend.lib.configSchema; }
        configSchema = builtins.fromJSON (builtins.readFile ./schema/tend-config.schema.json);
        configSchemaFile = ./schema/tend-config.schema.json;
      };
    };
}
