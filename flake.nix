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

      # ── THE `web` (wasm) VARIANT: THIS MODULE IS READY, THE PLATFORM IS NOT ─
      #
      # THE MODULE SIDE IS DONE (#167 Part A). Every outbound call site here is
      # the generated ASYNC client -- `modules().eth_rpc_module.call_async(...)`,
      # issued from `dispatch_multicall` in rust-lib/src/glue.rs, which is the
      # module's ONE outbound call site. The synchronous twin is not a spelling a
      # wasm image can have: a Worker is a single event loop with no ASYNCIFY
      # (ADR 0004), so a call that blocked for its reply would deadlock the loop
      # that delivers it. That rewrite is what the `start_get_prices` /
      # `start_quote_swap` / `start_build_swap` + `take_result` methods are --
      # the shape an async call has from the outside, on every target.
      #
      # WHAT IS STILL MISSING IS NOT IN THIS REPO. logos-protocol's wasm subset
      # (cpp/implementations/wasm/wasm_lp_abi.cpp) implements the token/inbound
      # half of the C ABI and not the outbound one, so an image linking this
      # crate still fails at `wasm-ld` naming `lp_client_create` /
      # `lp_client_destroy` / `lp_invoke_async`. Until that lands (#166), the
      # builder's dependency gate (#165) publishes no `web` output for a module
      # with dependencies at all, so there is nothing here to build and no
      # `checks.web-variant` to publish.
      #
      # WHEN #166 LANDS, this block goes and `checks.<system>.web-variant`
      # arrives with it: a node harness driving `start_get_prices` +
      # `take_result` against a stub `eth_rpc_module` answering the Multicall3
      # `eth_call`, modelled on logos-evm-keystore-module/nix/web-variant-test.nix.
      #
      # WHAT THE PHONE DOES MEANWHILE, unchanged: uniswap runs as a NATIVE
      # Bundled Bare module in the app image (the mobile keys above), and the
      # wallet UI's `web` variant calls it BY NAME over the view door -- the same
      # path it already takes to eth_rpc_module. No wasm uniswap is involved in a
      # working Market tab.
    };
}
