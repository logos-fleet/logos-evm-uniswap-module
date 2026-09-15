# THE `web` VARIANT, DRIVEN THROUGH ITS ONE OUTBOUND CALL.
#
# This module's whole job is one Multicall3 `eth_call` issued through
# `eth_rpc_module`, and inside a Wasm image that is the part that could not
# exist until logos-protocol grew an outbound door (#166). Everything the
# module's own unit tests cover — CREATE2 pool derivation, reserve maths,
# best-rate selection, Multicall3 encode/decode — is pure and runs the same
# everywhere. What is NOT pure, and what nothing could ask before this file, is:
#
#   * whether the image can CALL OUT AT ALL. A wasm image holds no credential
#     for its dependency at startup — the core pushes a module its own token and
#     its callers', never an outbound one — so the door runs a
#     `capability_module.requestModule` handshake before the target is dialled.
#     The drive answers that handshake, then answers the `eth_call`, and the
#     price that comes back is derived from the stub's reserves alone;
#
#   * whether the reply LANDS IN THE MODULE'S CALLBACK. The async client has no
#     return value, so the answer is assembled on the completion path and parked
#     on the job board for `take_result`. A `start_*` that answered a job id and
#     then never filled it would look identical from the outside until somebody
#     collected;
#
#   * whether the REFUSAL is honest. The wasm door refuses to forward a call it
#     was not granted — unlike the native client, which forwards tokenless — so
#     a capability_module that grants nothing must leave eth_rpc_module undialled
#     AND must tell the module so, naming the target, rather than leaving a job
#     pending for ever;
#
#   * and whether the WAITING spelling refuses here. `get_prices` /
#     `quote_swap` / `build_swap` block for their reply, which is safe on a
#     native host and impossible in a Worker (one event loop, no ASYNCIFY —
#     ADR 0004). On this target they must dispatch NOTHING and say which methods
#     to call instead; a call whose reply can never be collected is worse than
#     no call.
#
# THE HARNESS IS THE FAR SIDE OF THE CHANNEL, which is what makes all of that
# checkable without a browser, a container or a second module: the image's one
# message port is `Module.logosOut`, so a frame the image SENDS is an entry in a
# transcript and a reply is one `logos_wasm_deliver` away. `eth_rpc_module` is
# present here only as the contract its `.lidl` publishes; node answers for it,
# in the answer shape its own `ok_result` produces.
#
# THE NUMBERS ARE THE DOC-TEST'S. `doctests/uniswap-module-runtime.test.yaml`
# drives the NATIVE module against a mock JSON-RPC node holding 6,000,000 USDC
# against 2,000 WETH, and asserts $3,000. The same reserves are encoded here, so
# a wasm image that decoded differently from the native one would show up as a
# different price rather than as a passing test of a different fixture.
#
# WHAT IS NOT ASSERTED, deliberately: that a `configure` survives a page reload.
# `ConfigStore` persists with plain `std::fs`, which in an image reaches the
# durable medium only when somebody calls the storage barrier — so on this
# target a runtime chain override lives for the life of the page. The seeded
# defaults (Ethereum, Optimism, Arbitrum, Base) are unaffected, which is what
# the wallet reads; making an override durable is a change to the module's
# storage, not to its call sites, and it is not what this issue is about.
#
# WHAT DOES THE DRIVING is `web-variant-drive.js` beside this file. This
# derivation only builds the variant, puts the two beside each other and runs
# them.
{ pkgs, webVariant }:

pkgs.runCommand "uniswap-web-variant-tests" {
  nativeBuildInputs = [ pkgs.nodejs ];
} ''
  set -euo pipefail
  variant=${webVariant}/uniswap_module_web
  test -s "$variant/uniswap_module_wasm.js" || { echo "FAIL: no wasm host in the web variant"; exit 1; }

  # Side by side in the build directory: the harness requires the host by relative
  # path, and node resolves that against the SCRIPT's own directory. The image
  # itself is loaded by the glue relative to the same place.
  cp "$variant/uniswap_module_wasm.js" ./host.js
  cp "$variant/uniswap_module_wasm_image.wasm" ./uniswap_module_wasm_image.wasm
  cp ${./web-variant-drive.js} ./drive.js

  node drive.js
  mkdir -p $out
  echo ok > $out/result
''
