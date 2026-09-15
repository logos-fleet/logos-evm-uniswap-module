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

      # ── WHY THERE IS NO `web` (wasm) OUTPUT, AND WHAT WOULD GIVE IT ONE ───
      #
      # logos-module-builder publishes `packages.<system>.web` for every
      # `codegen.rust` module whose protocol pin carries a wasm subset, and this
      # module's crate DOES compile to wasm32-unknown-emscripten -- measured, the
      # archive builds and holds wasm objects. The image will not LINK:
      #
      #   wasm-ld: error: lib uniswap_module.a(logos_rust_sdk…rcgu.o):
      #            undefined symbol: lp_client_create
      #            undefined symbol: lp_client_destroy
      #            undefined symbol: lp_invoke
      #
      # Those three are the OUTBOUND consumer stack, and a wasm image has none:
      # logos-protocol's wasm subset (cpp/implementations/wasm/wasm_lp_abi.cpp)
      # deliberately implements the token half of the C ABI and not this half,
      # and says so -- "a wasm module that tried to lp_client_create() would fail
      # to LINK, naming the symbol, which is the honest answer for an image with
      # no transport it could dial". logos_wasm_host.cpp states the same rule
      # from the other side: "it makes no outbound calls -- a wasm module with
      # dependencies is a later slice".
      #
      # It is not a gap in this module, and nothing here can close it. Every one
      # of this module's price and swap methods is ONE eth_call issued through
      # `modules().eth_rpc_module`, synchronously, and that synchronous shape is
      # what the target forbids: a Worker is a single event loop with no ASYNCIFY
      # (ADR 0004), so a call that blocked for a reply would deadlock the loop
      # that delivers it. The door that exists -- logos_web_module_call.h, used
      # by a `ui_qml` backend image -- is async-only for exactly that reason.
      #
      # WHAT THE PHONE DOES INSTEAD, and why the mobile Bare build above is the
      # whole of what the wallet needed: uniswap runs as a NATIVE Bundled Bare
      # module in the app image, and the wallet UI's `web` variant calls it by
      # name over that async door -- the same path it already takes to
      # eth_rpc_module. No wasm uniswap is involved in a working Market tab.
      #
      # So this flake publishes no `checks.web-variant`. A check that built the
      # `web` output would be red on every pin until the outbound door lands, and
      # one that asserted the link error would go red the day it does.
    };
}
