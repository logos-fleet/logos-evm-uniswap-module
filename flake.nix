{
  description = "Logos Uniswap module — V2/V3/V4 price oracle (best-rate, Multicall3) + V2/V3 swap building. Multi-chain, configurable.";

  inputs = {
    logos-module-builder.url = "github:logos-co/logos-module-builder";

    # Dependency module. Its published `.lidl` contract drives the generated
    # `modules().eth_rpc_module` typed client used to issue the batched eth_call.
    # The `follows` makes it use the SAME module-builder as this module.
    eth_rpc_module = {
      url = "github:logos-co/logos-evm-eth-rpc-module";
      inputs.logos-module-builder.follows = "logos-module-builder";
    };
  };

  outputs = inputs@{ self, logos-module-builder, ... }:
    let
      nixpkgs = logos-module-builder.inputs.nixpkgs;
      systems = [ "aarch64-darwin" "x86_64-darwin" "aarch64-linux" "x86_64-linux" ];

      # x86_64-windows is a cross PSEUDO-SYSTEM the builder already understands
      # (logos-module-builder lib/common.nix routes it to
      # logos-nix.lib.mkWindowsPkgs, and picks the build platform separately).
      # It is a target, never a host we evaluate nixpkgs natively for, so it
      # only ever belongs in `packages`.
      targets = systems ++ [ "x86_64-windows" ];

      # ONE module, answered for every target at once — mkLogosModule already
      # keys its own outputs by system, so calling it per target evaluated the
      # same module five times over and threw four away.
      module = logos-module-builder.lib.mkLogosModule {
        src = ./.;
        configFile = ./metadata.json;
        flakeInputs = inputs;
      };

      # The mobile pseudo-systems logos-nix keys its cross package sets by. Kept
      # out of `targets` above for the reason the builder keeps them out of its
      # own: a phone gets the Bare image and none of the other outputs.
      #
      # THIS IS WHAT MAKES uniswap BUNDLABLE (#148). A phone's Bundled set is
      # resolved out of a catalog whose every entry is a module's own
      # `mobile.<target>.bare`, so a module with no mobile output cannot be in
      # that set however well it builds on a desktop — and the wallet UI's `web`
      # variant then has nothing to ask for a price, which is why its Market tab
      # printed "uniswap_module ... has no mobile build".
      #
      # NOTHING HAD TO CHANGE IN THE MODULE to cross. Its crate is `alloy` with
      # default features off (`std` + `sol-types`), i.e. ABI encoding and 256-bit
      # arithmetic: no network, no C dependency, no `nix.external_libraries`. The
      # one eth_call it issues goes out through `modules().eth_rpc_module`, which
      # is a native Bare module beside it on the phone.
      #
      # `? ${t}` rather than a bare index, so a logos-module-builder pin without
      # the mobile cross sets leaves this flake simply WITHOUT mobile keys
      # instead of failing to evaluate.
      mobileTargets = builtins.filter (t: module.packages ? ${t})
        [ "aarch64-ios" "aarch64-ios-simulator" "aarch64-android" ];
    in
    {
      packages = nixpkgs.lib.genAttrs (targets ++ mobileTargets)
        (target: module.packages.${target});

      # An Android cross derivation's `system` is its BUILD platform, so
      # `packages.aarch64-android` is pinned to the builder's canonical one
      # (x86_64-linux) and a Mac cannot realise it. The same artifact, reached
      # from whichever machine is doing the building:
      #   nix build .#legacyPackages.aarch64-darwin.mobile.aarch64-android.bare
      legacyPackages = module.legacyPackages or { };

      # THE MODULE'S OWN ANSWER ABOUT ITSELF, forwarded so a consumer flake can
      # read it without building anything. logos-basecamp's mobile catalog takes
      # this module's `version` and, more importantly, its `dependencies` from
      # here rather than restating them: a Bundled set resolves a CLOSURE out of
      # the catalog entry, so `--bundle uniswap_module` has to pull
      # eth_rpc_module in without naming it, and a hand-copied list in a SIGNED
      # manifest is a claim the core would act on after it had drifted.
      # `configFor` is the per-target resolution of the same document; this
      # module has no `platforms` overlay, so the two agree everywhere.
      inherit (module) config configFor;

      # ── THE `web` (wasm) VARIANT, AND THE CHECK THAT DRIVES IT ─────────────
      #
      # `packages.<system>.web` is an emscripten image with the module's crate,
      # logos-protocol's wasm subset and a Wasm host linked into it — the form
      # this module takes in a webview, where there is no dlopen and no host to
      # dlopen into (ADR 0003). It exists for a module with DEPENDENCIES only
      # because logos-protocol's wasm subset now implements the outbound half of
      # the C ABI (`lp_client_create` / `lp_invoke_async`), which is what
      # logos-module-builder's gate reads off the pin (`hasOutboundDoor`,
      # ADR 0009 gate 2).
      #
      # The one outbound call site here is the generated ASYNC client —
      # `modules().eth_rpc_module.call_async(...)` from `dispatch_multicall` in
      # rust-lib/src/glue.rs. The synchronous twin is not a spelling a wasm
      # image can have: a Worker is a single event loop with no ASYNCIFY
      # (ADR 0004), so a call that blocked for its reply would deadlock the loop
      # that delivers it — which is what `start_get_prices` / `start_quote_swap`
      # / `start_build_swap` + `take_result` are for, and why `get_prices` and
      # its twins refuse on this target instead of dispatching.
      #
      # forAllSystems, not forAllTargets: a check is BUILT and RUN here, and
      # x86_64-windows is a cross target this machine cannot run.
      #
      # A SKIP THAT SAYS SO when the builder publishes no `web` output for this
      # module — a pin whose logos-protocol has no wasm outbound door, or one
      # from before the builder could compile a Rust core to wasm32 at all. That
      # is a pin rollout, not a defect, and an absent check would be a green run
      # with a silently missing test.
      checks = nixpkgs.lib.genAttrs systems (system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          modulePkgs = module.packages.${system};
        in {
          web-variant =
            if modulePkgs ? web
            then import ./nix/web-variant-test.nix { inherit pkgs; webVariant = modulePkgs.web; }
            else pkgs.runCommand "uniswap-web-variant-tests-skipped" { } ''
              echo "SKIP: web-variant -- this pin publishes no \`web\` output for"
              echo "      uniswap_module. A module with dependencies gets one only"
              echo "      when logos-protocol's wasm subset carries the outbound"
              echo "      door (hasOutboundDoor). Run through the workspace flake:"
              echo "        ws test logos-evm-uniswap-module --auto-local"
              mkdir -p $out
              echo skipped > $out/result
            '';
        });

      # WHAT THE PHONE DOES, unchanged by any of the above: uniswap runs as a
      # NATIVE Bundled Bare module in the app image (the mobile keys above), and
      # the wallet UI's `web` variant calls it BY NAME over the view door — the
      # same path it already takes to eth_rpc_module. No wasm uniswap is involved
      # in a working Market tab.
    };
}
