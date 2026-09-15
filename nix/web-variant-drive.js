// THE CONTAINER'S JOB AND eth_rpc_module's, BOTH DONE IN NODE.
//
// `nix/web-variant-test.nix` builds the `web` variant, puts this file beside its
// wasm host and runs it; the header there says what this asserts and why the
// harness has to be the far side of the wire rather than a second module.
//
// Every frame below is exactly what the Web container puts on the wire — a
// {type, payload} envelope with logos-protocol's MessageType tags — so nothing
// here stands in for the transport, only for the browser and for the dependency.
'use strict';

const factory = require('./host.js');

const CALL = 1, RESULT = 2, METHODS = 7, METHODS_RESULT = 8;

const fail = (why, transcript) => {
  console.error('FAIL: ' + why);
  if (transcript) console.error(JSON.stringify(transcript, null, 2));
  process.exit(1);
};

// ── the fixture chain ───────────────────────────────────────────────────────
//
// The doc-test's local chain, verbatim (doctests/uniswap-module-runtime.test.yaml):
// a V2-only deployment with USDC as the stablecoin, which is the smallest config
// that prices a token from ONE pool read. `v2Router` is added because this drive
// goes further than the doc-test does and quotes and builds a swap as well.
const CHAIN = {
  chainId: 31337,
  weth: '0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2',
  stablecoins: ['0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48'],
  multicall3: '0xcA11bde05977b3631167028862bE2a173976CA11',
  v2Factory: '0x5C69bEe701ef814a2B6a3EDD4B1652CB9cc5aA6f',
  v2InitCodeHash: '0x96e8ac4277198ff8b6f785478aa9a39f403cb768dd02cbee326c3e7da348845f',
  v2Router: '0x7a250d5630B4cF539739dF2C5dAcb4c659F2488D',
};
// Read back off CHAIN rather than restated, so the fixture cannot drift from
// itself: the token the drive prices IS the chain's stablecoin.
const USDC = CHAIN.stablecoins[0];
const CHAIN_ID = CHAIN.chainId;

// `aggregate3((address,bool,bytes)[])` — the one batch selector this module emits.
const AGGREGATE3 = '0x82ad56cb';

// What the capability handshake below mints, and therefore what every outbound
// frame that follows it has to carry.
const GRANTED_TOKEN = 'tok-for-eth-rpc';

// ── what the stub node answers ──────────────────────────────────────────────
//
// Multicall3's `aggregate3` returns `(bool success, bytes returnData)[]`, and
// the module decodes exactly that. Encoded here rather than pasted as a hex
// blob so the two swap fixtures — whose return types are not the pricing one —
// can be written as the numbers they are.
const word = (n) => BigInt(n).toString(16).padStart(64, '0');

const bytesArg = (hex) => {
  const body = hex.replace(/^0x/, '');
  const len = body.length / 2;
  return word(len) + body + '0'.repeat(((32 - (len % 32)) % 32) * 2);
};

// `(bool,bytes)[]` as the whole return of a call: one head offset to the array,
// then the array. Each element is dynamic (it holds `bytes`), so the array's
// body is a table of offsets followed by the tuples themselves.
const aggregate3Returns = (entries) => {
  const tuples = entries.map((e) =>
    word(e.success === false ? 0 : 1) + word(0x40) + bytesArg(e.data || '0x'));
  let offsets = '', body = '', at = entries.length * 32;
  for (const t of tuples) {
    offsets += word(at);
    body += t;
    at += t.length / 2;
  }
  return '0x' + word(0x20) + word(entries.length) + offsets + body;
};

// A Uniswap V2 pair's `getReserves()` — reserve0, reserve1, blockTimestampLast.
const getReserves = (r0, r1) => '0x' + word(r0) + word(r1) + word(0);

// `getAmountsOut` → `uint256[] amounts`; the module takes the last hop.
const amountsOut = (amounts) =>
  '0x' + word(0x20) + word(amounts.length) + amounts.map(word).join('');

// 6,000,000 USDC against 2,000 WETH: 1 WETH = 3,000 USDC. The price the module
// reports below is derived from exactly these reserves, which is what makes the
// assertion a number and not a shape.
const RESERVES = getReserves(6000000n * 10n ** 6n, 2000n * 10n ** 18n);
const RESERVES_REPLY = aggregate3Returns([{ data: RESERVES }]);

// 1,000 USDC in; a third of an ETH out. Arbitrary, and therefore checkable: it
// can only reach the answer by being decoded out of this frame.
const AMOUNT_IN = 1000n * 10n ** 6n;
const AMOUNT_OUT = 333333333333333333n;
const QUOTE_REPLY = aggregate3Returns([{ data: amountsOut([AMOUNT_IN, AMOUNT_OUT]) }]);

// eth_rpc_module's own answer shape (`ok_result` in its glue.rs): the module
// reads `ok` and `result` off it, so a stub that answered a bare hex string
// would be testing a decoder nobody has.
const rpcResult = (hex) => JSON.stringify({ ok: true, result: hex, route: 'direct' });

// ── one image, with its message port wired to an array ──────────────────────
//
// A second call to this is the page after a reload, and — more to the point
// here — an image that has NOT been granted a credential yet.
async function spawn() {
  const heard = [];
  let hello = null;
  const mod = await factory({
    logosOut: (text) => {
      let msg;
      try { msg = JSON.parse(text); } catch { return; }
      if (msg.logosWasmHost) hello = msg; else heard.push(msg);
    },
    print: () => {},
    printErr: (s) => console.error('[image] ' + s),
  });
  const deliver = mod.cwrap('logos_wasm_deliver', null, ['string']);
  let nextId = 0;
  let answered = 0;

  const image = {
    heard,
    hello: () => hello,
    send: (type, payload) => deliver(JSON.stringify({ type, payload })),
    // Frames the IMAGE sent, i.e. its outbound calls.
    outbound: () => heard.filter((m) => m.type === CALL),
    // ...and the ones no `answer*` helper has replied to yet. The drive answers
    // them strictly in order, so a step inserted below does not renumber the
    // steps after it.
    unanswered: () => image.outbound().slice(answered),
    take: () => {
      const frame = image.unanswered()[0];
      if (frame) answered += 1;
      return frame;
    },
    // Answers are synchronous on this transport: the image is driven on this
    // thread and the RESULT is already in `heard` when deliver() returns.
    call: (method, args) => {
      const id = ++nextId;
      image.send(CALL, { id, authToken: '', object: 'uniswap_module', method, args });
      const res = heard.find((m) => m.type === RESULT && m.payload.id === id);
      if (!res) fail(method + ' did not answer at all', heard);
      if (!res.payload.ok) fail(method + ' failed at the transport: ' + res.payload.err, heard);
      return res.payload.value;
    },
    json: (method, args) => {
      const raw = image.call(method, args);
      try { return JSON.parse(raw); } catch { fail(method + ' answered malformed JSON: ' + raw); }
    },
    // The interface the image publishes. Matched on TYPE rather than id: a
    // MethodsResult can only be the answer to the one Methods frame sent here.
    methods: () => {
      const id = ++nextId;
      image.send(METHODS, { id, authToken: '', object: 'uniswap_module' });
      const res = heard.find((m) => m.type === METHODS_RESULT);
      if (!res || !res.payload.ok) fail('the image answered no Methods', heard);
      return res.payload.methods;
    },
  };
  return image;
}

// The handshake the door runs the first time it dials a target it holds no
// credential for, answered with `grant`. `""` grants nothing, which is the
// refusal case.
function answerHandshake(image, grant) {
  const ask = image.take();
  if (!ask || ask.payload.object !== 'capability_module'
      || ask.payload.method !== 'requestModule') {
    fail('the image did not ask capability_module for a token', image.heard);
  }
  if (ask.payload.args[1] !== 'eth_rpc_module') {
    fail('the handshake did not name the target: ' + JSON.stringify(ask.payload.args));
  }
  image.send(RESULT, { id: ask.payload.id, ok: true, value: grant });
}

// The one outbound call this module makes, asserted frame by frame, and
// answered with `replyHex`. Returns nothing: what it is for is the assertions.
function answerEthCall(image, replyHex) {
  const call = image.take();
  if (!call || call.payload.object !== 'eth_rpc_module' || call.payload.method !== 'call') {
    fail('the image did not issue its eth_call: '
         + JSON.stringify(image.outbound().map((m) => m.payload.object + '.' + m.payload.method)),
         image.heard);
  }
  if (call.payload.authToken !== GRANTED_TOKEN) {
    fail('the outbound frame carries authToken=' + JSON.stringify(call.payload.authToken)
         + ', not the credential capability_module granted');
  }
  const args = call.payload.args;
  if (args[0] !== CHAIN_ID) fail('the eth_call went to chain ' + args[0]);
  // `deadline_ms` is Option<i64> and the module passes None: the protocol's own
  // 20s default governs, which is the decision Part A recorded.
  if (args[2] !== null) fail('the eth_call carries a deadline it should not: ' + args[2]);
  const eth = JSON.parse(args[1]);
  if (eth.to !== CHAIN.multicall3) {
    fail('the batch was not addressed to the configured Multicall3: ' + eth.to);
  }
  if (!eth.data.startsWith(AGGREGATE3)) {
    fail('the batch is not an aggregate3 call: ' + eth.data.slice(0, 10));
  }
  image.send(RESULT, { id: call.payload.id, ok: true, value: rpcResult(replyHex) });
}

// How many frames the image has sent that nothing has answered. `0` is "the
// image dispatched nothing"; `1` is "exactly the one call this step expects, and
// no handshake in front of it".
function expectUnanswered(image, count, why) {
  const pending = image.unanswered();
  if (pending.length !== count) {
    fail(why + ': ' + JSON.stringify(pending.map((m) => m.payload.object + '.' + m.payload.method)),
         image.heard);
  }
}

const near = (a, b) => typeof a === 'number' && Math.abs(a - b) < b / 1000;

(async () => {
  const a = await spawn();

  // ── the image is a live uniswap_module ────────────────────────────────────
  const hello = a.hello();
  if (!hello || hello.logosWasmHost !== 'uniswap_module') {
    fail('the image did not announce itself: ' + JSON.stringify(hello));
  }
  const names = a.methods().map((m) => m.name).sort();
  for (const want of ['build_swap', 'configure', 'get_chains', 'get_prices', 'quote_swap',
                      'start_build_swap', 'start_get_prices', 'start_quote_swap',
                      'take_result']) {
    if (!names.includes(want)) fail('the published interface is missing ' + want + ': ' + names);
  }

  // `on_context_ready` ran: the seeded multi-chain map is there — 8453 is Base,
  // one of the four defaults — which is the module's own state and not something
  // the host could have answered for it.
  const chains = a.json('get_chains', []);
  if (!chains.ok || !(chains.chains || []).some((c) => c.chainId === 8453)) {
    fail('the image has no config store -- on_context_ready did not run: '
         + JSON.stringify(chains).slice(0, 200));
  }
  console.log('PASS: the image serves uniswap_module and seeded its chain map');

  // ── THE ACCEPTANCE CRITERION: get_prices, end to end ─────────────────────
  if (a.call('configure', [JSON.stringify(CHAIN)]) !== true) {
    fail('configure did not take');
  }
  const TOKENS = JSON.stringify({ tokens: [{ address: USDC, decimals: 6 }] });

  // THE WAITING SPELLING IS REFUSED HERE, AND SAYS WHAT TO CALL INSTEAD.
  //
  // One Worker, one event loop, no ASYNCIFY (ADR 0004): the thread that would
  // wait for the reply is the thread that must deliver it. `get_prices` does not
  // dispatch on this target — a call whose reply can never be collected is worse
  // than none — and the whole point of the refusal is that it names the methods
  // that do work, so a caller reading the error can act on it. Asked AFTER
  // `configure`, so that what refuses it is the target and not a missing chain.
  const waited = a.json('get_prices', [CHAIN_ID, TOKENS]);
  if (waited.ok !== false || !/start_get_prices/.test(waited.error || '')) {
    fail('the waiting spelling did not refuse on wasm: ' + JSON.stringify(waited));
  }
  expectUnanswered(a, 0, 'the waiting spelling dispatched a call it can never collect');
  console.log('PASS: get_prices refuses on a `web` image and names the async spelling');

  const started = a.json('start_get_prices', [CHAIN_ID, TOKENS]);
  if (!started.ok || !started.jobId) fail('start_get_prices: ' + JSON.stringify(started));

  // ASYNC, AND THE TRANSCRIPT SAYS SO: the call is on the wire and nothing has
  // answered it, so the job has no answer to give.
  const pending = a.json('take_result', [started.jobId]);
  if (pending.pending !== true) {
    fail('the job answered before the dependency did -- the call is not async: '
         + JSON.stringify(pending));
  }

  // The image holds no credential for its dependency at startup — the core
  // pushes a module its OWN token and its callers', never an outbound one — so
  // the door runs the capability handshake before the target is dialled at all.
  answerHandshake(a, GRANTED_TOKEN);
  answerEthCall(a, RESERVES_REPLY);

  const priced = a.json('take_result', [started.jobId]);
  if (!priced.ok || priced.chainId !== CHAIN_ID) fail('take_result: ' + JSON.stringify(priced));
  const byAddress = Object.fromEntries((priced.prices || []).map((p) => [p.address, p]));
  if (!near(byAddress.ETH?.usd, 3000)) {
    fail('ETH is not $3000 against the stub reserves: ' + JSON.stringify(priced.prices));
  }
  if (!near(byAddress[USDC]?.usd, 1)) {
    fail('USDC is not $1 against the stub reserves: ' + JSON.stringify(priced.prices));
  }
  if (!near(byAddress[USDC]?.eth, 1 / 3000)) {
    fail('USDC/ETH is not 1/3000: ' + JSON.stringify(priced.prices));
  }
  console.log('PASS: a `web` image priced a token from one Multicall3 batch '
              + '(ETH $' + byAddress.ETH.usd + ', USDC $' + byAddress[USDC].usd + ')');

  // ONCE, and the board says so rather than leaving a poller on a slot that
  // will never fill.
  const again = a.json('take_result', [started.jobId]);
  if (again.ok !== false || again.pending === true) {
    fail('a collected job answered twice: ' + JSON.stringify(again));
  }
  console.log('PASS: a job is collected once, and a second collect is an error');

  // ── ONCE PER TARGET: the grant is remembered ─────────────────────────────
  //
  // ...and the swap path is the same door. `start_quote_swap` goes straight to
  // eth_rpc_module, carrying the token the handshake above minted.
  const quoting = a.json('start_quote_swap', [CHAIN_ID, JSON.stringify({
    tokenIn: USDC, tokenOut: 'ETH', amountIn: AMOUNT_IN.toString() })]);
  if (!quoting.ok || !quoting.jobId) fail('start_quote_swap: ' + JSON.stringify(quoting));
  expectUnanswered(a, 1, 'the second target dial re-ran the handshake');
  answerEthCall(a, QUOTE_REPLY);

  const quote = a.json('take_result', [quoting.jobId]);
  if (!quote.ok || quote.version !== 'V2' || quote.amountOut !== AMOUNT_OUT.toString()) {
    fail('the V2 quote did not come back out of the stub reply: ' + JSON.stringify(quote));
  }
  console.log('PASS: the capability handshake runs once per target, and a V2 quote '
              + 'decodes in wasm (' + quote.amountOut + ' wei)');

  // ── A BUILD IS ITS QUOTE'S CALL, plus offline calldata ───────────────────
  //
  // One outbound call, not two: the router calldata and the ERC20 approval are
  // encoded from the same reply, which is why nothing in this module has to
  // reason about reply ORDER.
  const building = a.json('start_build_swap', [CHAIN_ID, JSON.stringify({
    tokenIn: USDC, tokenOut: 'ETH', amountIn: AMOUNT_IN.toString(),
    recipient: '0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266' })]);
  if (!building.ok || !building.jobId) fail('start_build_swap: ' + JSON.stringify(building));
  answerEthCall(a, QUOTE_REPLY);
  expectUnanswered(a, 0, 'building a swap took more than the quote it is built on');

  const built = a.json('take_result', [building.jobId]);
  if (!built.ok || built.router !== CHAIN.v2Router) {
    fail('the swap was not built for the configured V2 router: ' + JSON.stringify(built));
  }
  // swapExactTokensForETH(uint256,uint256,address[],address,uint256).
  if (!String(built.data).startsWith('0x18cbafe5')) {
    fail('the router calldata is not swapExactTokensForETH: '
         + String(built.data).slice(0, 10));
  }
  // An ERC20 input needs its approval to land first, and the backend has to be
  // told so — 0x095ea7b3 is `approve(address,uint256)`.
  if (!built.approve || built.approve.spender !== CHAIN.v2Router
      || !String(built.approve.data).startsWith('0x095ea7b3')) {
    fail('the build did not carry the ERC20 approval: ' + JSON.stringify(built.approve));
  }
  // amountOutMin is the quote minus the default 0.5% slippage.
  const wantMin = (AMOUNT_OUT * 9950n) / 10000n;
  if (built.amountOutMin !== wantMin.toString()) {
    fail('slippage was not applied to the quote: ' + built.amountOutMin
         + ', expected ' + wantMin);
  }
  console.log('PASS: a swap was built in wasm over the same reply, with its approval '
              + 'and its slippage bound');

  // ── THE REFUSAL, which is the security-relevant half ─────────────────────
  //
  // A fresh image, because the one above now holds a credential. The door
  // REFUSES to forward a call it was not granted — unlike the native client,
  // which forwards tokenless — so a capability_module that grants nothing
  // leaves eth_rpc_module undialled, and the module hears about it in its own
  // callback rather than from whatever the far side happens to check.
  const b = await spawn();
  if (b.call('configure', [JSON.stringify(CHAIN)]) !== true) fail('configure did not take (b)');
  const refusedJob = b.json('start_get_prices', [CHAIN_ID, TOKENS]);
  if (!refusedJob.ok || !refusedJob.jobId) fail('start_get_prices (b): ' + JSON.stringify(refusedJob));
  answerHandshake(b, '');
  expectUnanswered(b, 0, 'the target was dialled without a token');
  const refused = b.json('take_result', [refusedJob.jobId]);
  if (refused.ok !== false || !/eth_rpc_module/.test(refused.error || '')) {
    fail('an ungranted call was not reported to the module as a failure naming the '
         + 'target: ' + JSON.stringify(refused), b.heard);
  }
  console.log('PASS: a target this image holds no token for is refused, not forwarded, '
              + 'and the job carries the refusal');
})().catch((e) => fail('the harness threw: ' + ((e && e.stack) || e)));
