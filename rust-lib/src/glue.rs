//! Logos module glue for `uniswap_module` (rust-first authoring).
//!
//! Depends on `eth_rpc_module` (declared in metadata.json `dependencies`),
//! reached as `modules().eth_rpc_module.call_async(chainId, callJson, deadlineMs,
//! callback)`. This module is the wallet's **price oracle and swap router**: it
//! derives pool addresses offline, bundles every on-chain read into one
//! Multicall3 `eth_call`, and returns token→ETH / token→USD prices and
//! best-rate swap quotes/transactions.
//!
//! Compiled only with the default `logos_module` feature; the pure cores
//! (`config`, `jobs`, `pricing`, `swap`) are tested with
//! `cargo test --no-default-features`.
//!
//! ── THE OUTBOUND CALL IS ASYNC, AND THERE IS EXACTLY ONE OF THEM ────────────
//!
//! Every price/quote/swap method is ONE Multicall3 `eth_call`, and it is issued
//! through the generated `_async` client — `dispatch_multicall` below is the
//! module's only outbound call site. The synchronous twin is not a spelling a
//! `web` (wasm) image can have: a Worker is a single event loop with no
//! ASYNCIFY (ADR 0004), so a call that blocked for its reply would deadlock the
//! loop that delivers it, and logos-rust-sdk does not compile `lp_invoke` there
//! at all.
//!
//! An async call has no return value, which is what the shape of this file is
//! about. Each method is split in three:
//!
//!   plan_*    — read config, derive pool addresses, build the batch. No network.
//!   dispatch  — the one `call_async`.
//!   finish_*  — decode the reply into the answer JSON. Pure, and it runs in the
//!               callback, on another thread, after the method that asked
//!               returned. It therefore carries its plan rather than re-reading
//!               module state.
//!
//! and is then offered in two spellings:
//!
//!   `get_prices` / `quote_swap` / `build_swap`   — answer the result directly.
//!       NATIVE ONLY. They wait for the reply on a channel, which is safe here
//!       and nowhere else: `concurrency: "multi"` puts every dispatch on its own
//!       worker QThread, while `lp_client_create` anchors the consumer on the Qt
//!       MAIN thread (logos_protocol.cpp: "we construct on the Qt main thread
//!       rather than on whichever thread happened to call first") and
//!       `invokeRemoteMethodAsync` marshals the completion onto that owner
//!       thread. So the thread that waits is never the thread that must deliver.
//!       On a `web` image there is only one thread and both would be it, so
//!       these report that and dispatch nothing.
//!
//!   `start_get_prices` / `start_quote_swap` / `start_build_swap` + `take_result`
//!       — the shape the platform actually has, everywhere. `start_*` fires the
//!       call and answers a job id at once; the answer is parked on the
//!       `jobs::JobBoard` by the callback and collected by `take_result`.
//!
//! NO METHOD HERE CHAINS TWO OUTBOUND CALLS, which is why nothing in this file
//! has to reason about reply ORDER (replies are not ordered). `build_swap` looks
//! like two steps — quote, then build — but only the quote leaves the process;
//! the build is offline calldata over the same plan.
//!
//! `concurrency: "multi"` (metadata.json): several chains can be priced at once
//! without serializing. The multi contract makes the generated trait take `&self`
//! + `Send + Sync`; the config map lives behind a `RwLock` (read it, clone the
//! chain, drop the lock, then call — `configure` is the only writer).

use std::sync::RwLock;
use std::time::{SystemTime, UNIX_EPOCH};

use alloy::primitives::{Address, U256};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::config::{ChainUniswap, ConfigStore, STABLE_DECIMALS};
use crate::jobs::{Job, JobBoard};
use crate::pricing::{self, parse_addr as parse_addr_opt};
use crate::swap;

pub trait UniswapModule: Send + Sync + 'static {
    /// Add or override a chain's Uniswap config (JSON of `ChainUniswap`).
    fn configure(&self, chain_json: String) -> bool;
    /// All configured chains (defaults + overrides).
    fn get_chains(&self) -> String;
    /// Token→ETH and token→USD prices for `{ "tokens": [{address, decimals}] }`,
    /// best-rate across V2/V3/V4, batched into one Multicall3 `eth_call`.
    ///
    /// Waits for the reply, so it answers the prices directly. Not available on
    /// a `web` (wasm) image — use [`Self::start_get_prices`] + [`Self::take_result`],
    /// which work everywhere.
    fn get_prices(&self, chain_id: i64, tokens_json: String) -> String;
    /// Best swap quote for `{ tokenIn, tokenOut, amountIn }` (native = "ETH").
    /// Waits for the reply; see [`Self::start_quote_swap`] for the async twin.
    fn quote_swap(&self, chain_id: i64, params_json: String) -> String;
    /// Unsigned swap tx (router, value, data, +approval) for the best route.
    /// Waits for the reply; see [`Self::start_build_swap`] for the async twin.
    fn build_swap(&self, chain_id: i64, params_json: String) -> String;

    /// [`Self::get_prices`] without waiting: answers `{ok, jobId}` at once and
    /// parks the prices for [`Self::take_result`].
    fn start_get_prices(&self, chain_id: i64, tokens_json: String) -> String;
    /// [`Self::quote_swap`] without waiting. See [`Self::start_get_prices`].
    fn start_quote_swap(&self, chain_id: i64, params_json: String) -> String;
    /// [`Self::build_swap`] without waiting. See [`Self::start_get_prices`].
    fn start_build_swap(&self, chain_id: i64, params_json: String) -> String;
    /// Collect a `start_*` job: `{ok:false, pending:true}` while the `eth_call`
    /// is in flight, then the answer — ONCE. A second collect, an id that was
    /// never started, and one evicted at `jobs::CAPACITY` all report an error,
    /// so a poller is never left waiting on a slot that will never fill.
    fn take_result(&self, job_id: String) -> String;

    fn on_context_ready(&self, _ctx: &RustModuleContext) {}
}

include!(concat!(env!("CARGO_MANIFEST_DIR"), "/generated/provider_gen.rs"));

/// Answers parked while their `eth_call` is in flight.
///
/// A `static`, not a field on the impl: the callback `call_async` takes is
/// `FnOnce + Send + 'static` and cannot borrow the module. That is true of
/// every async callback in the SDK, not a property of this module.
static JOBS: JobBoard = JobBoard::new();

/// How long a waiting method gives the reply.
///
/// Deliberately LONGER than the protocol's own 20s default so that a call which
/// times out is reported by the protocol, with its cause, rather than by this
/// bare stopwatch.
#[cfg(not(target_os = "emscripten"))]
const REPLY_BUDGET: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Default)]
struct UniswapModuleImpl {
    cfg: RwLock<Option<ConfigStore>>,
}

// ── helpers ──────────────────────────────────────────────────────────────────

fn err(e: impl std::fmt::Display) -> String {
    json!({ "ok": false, "error": e.to_string() }).to_string()
}

/// Native ETH is "ETH"/""/`0x0…0`; everything else is a 20-byte address.
fn parse_token(s: &str) -> Result<Address, String> {
    let t = s.trim();
    if t.is_empty() || t.eq_ignore_ascii_case("eth") || t.eq_ignore_ascii_case("native") {
        return Ok(Address::ZERO);
    }
    parse_addr_opt(t).ok_or_else(|| format!("invalid address: {s}"))
}

fn parse_u256(s: &str) -> U256 {
    let t = s.trim();
    if let Some(h) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        U256::from_str_radix(h, 16).unwrap_or(U256::ZERO)
    } else {
        t.parse().unwrap_or(U256::ZERO)
    }
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[derive(Deserialize)]
struct TokenIn {
    address: String,
    #[serde(default = "default_decimals")]
    decimals: u8,
}
fn default_decimals() -> u8 {
    18
}

#[derive(Deserialize)]
struct PricesReq {
    #[serde(default)]
    tokens: Vec<TokenIn>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SwapReq {
    token_in: String,
    token_out: String,
    amount_in: String,
    #[serde(default)]
    amount_out_min: String,
    #[serde(default)]
    recipient: String,
    #[serde(default)]
    deadline: u64,
    #[serde(default = "default_slippage_bps")]
    slippage_bps: u64,
}
fn default_slippage_bps() -> u64 {
    50 // 0.5%
}

// ── the outbound call ────────────────────────────────────────────────────────

/// The eth_rpc `call` argument for a Multicall3 batch. `None` = nothing to ask,
/// which is not an error: a chain with no configured Uniswap deployment prices
/// nothing and still answers.
fn multicall_request(multicall3: &str, calls: &[(Address, Vec<u8>)]) -> Option<String> {
    if calls.is_empty() {
        return None;
    }
    let data = pricing::multicall3_aggregate3_calldata(calls);
    Some(json!({ "to": multicall3, "data": format!("0x{}", hex::encode(data)) }).to_string())
}

/// Turn one eth_rpc `call` reply into the per-sub-call Multicall3 returns.
/// Pure, so the waiting path and the callback path decode identically.
fn decode_multicall_reply(resp: &str) -> Result<Vec<Option<Vec<u8>>>, String> {
    let v: Value = serde_json::from_str(resp).map_err(|e| e.to_string())?;
    if v.get("ok").and_then(Value::as_bool) == Some(false) {
        return Err(v.get("error").and_then(Value::as_str).unwrap_or("eth_call failed").to_string());
    }
    let result_hex = v.get("result").and_then(Value::as_str).ok_or("multicall: no result")?;
    let bytes = hex::decode(result_hex.trim_start_matches("0x")).map_err(|e| e.to_string())?;
    pricing::decode_aggregate3_returns(&bytes).ok_or_else(|| "multicall: decode failed".to_string())
}

/// THE MODULE'S ONE OUTBOUND CALL SITE. Issue `aggregate3(calls)` through
/// eth_rpc and hand the per-call results to `done`.
///
/// `done` runs on the protocol's completion path (the consumer's owner thread),
/// not on the thread that called this — which is why it is `Send + 'static` and
/// why it carries everything it needs by value.
fn dispatch_multicall<F>(chain_id: i64, request: Option<String>, done: F)
where
    F: FnOnce(Result<Vec<Option<Vec<u8>>>, String>) + Send + 'static,
{
    let Some(call_json) = request else {
        return done(Ok(Vec::new()));
    };
    modules().eth_rpc_module.call_async(chain_id, &call_json, None, move |reply| {
        done(reply.map_err(|e| e.to_string()).and_then(|resp| decode_multicall_reply(&resp)))
    });
}

/// Fire the call and WAIT for its answer.
///
/// Only on a target where the waiting thread is not the delivering thread; see
/// this file's header. `concurrency: "multi"` is load-bearing: without it the
/// dispatch would run on the same Qt main thread the completion is marshalled
/// onto and this would hang for the full budget.
#[cfg(not(target_os = "emscripten"))]
fn await_multicall(chain_id: i64, request: Option<String>) -> Result<Vec<Option<Vec<u8>>>, String> {
    let (tx, rx) = std::sync::mpsc::channel();
    dispatch_multicall(chain_id, request, move |result| {
        let _ = tx.send(result);
    });
    rx.recv_timeout(REPLY_BUDGET)
        .unwrap_or_else(|_| Err("eth_rpc did not answer within the call budget".to_string()))
}

/// The same entry point on a `web` (wasm) image, where waiting is the one thing
/// that cannot be done: one Worker, one event loop, no ASYNCIFY (ADR 0004), so
/// the thread that would wait is the thread that must deliver. Nothing is
/// dispatched — a call whose reply can never be collected is worse than none.
#[cfg(target_os = "emscripten")]
fn await_multicall(chain_id: i64, request: Option<String>) -> Result<Vec<Option<Vec<u8>>>, String> {
    let _ = (chain_id, request);
    Err("this build cannot wait for a reply (one Worker, one event loop): \
         use start_get_prices / start_quote_swap / start_build_swap, then take_result"
        .to_string())
}

// ── plans: what a method knows before the network, and what it does after ────

/// Everything `get_prices` resolved before it needed the network. Carried into
/// the callback so the answer is assembled without touching module state again
/// — by then the method that asked has returned and the config lock is gone.
struct PricesPlan {
    chain_id: i64,
    multicall3: String,
    weth: Address,
    stable_addrs: Vec<Address>,
    /// The caller's tokens, echoed back in the answer under the spelling they
    /// used. Only the ones that parsed, which is what the synchronous version
    /// reported too.
    requested: Vec<(String, Address)>,
    batch: pricing::PricingBatch,
}

/// The quote batch plus the chain it was built from — kept so `build_swap` does
/// not read the config a second time after its reply lands.
struct QuotePlan {
    chain_id: i64,
    chain: ChainUniswap,
    batch: swap::QuoteBatch,
}

/// A quote plan plus the swap the caller asked to build from it.
struct BuildPlan {
    quote: QuotePlan,
    token_in: Address,
    token_out: Address,
    amount_in: U256,
    /// `None` = derive it from the quote and `slippage_bps`.
    amount_out_min: Option<U256>,
    slippage_bps: u64,
    recipient: Address,
    deadline: U256,
}

fn finish_prices(plan: &PricesPlan, results: &[Option<Vec<u8>>]) -> String {
    let eth_prices = pricing::decode_prices(&plan.batch, results);
    let usd_prices = pricing::token_usd_prices(&eth_prices, plan.weth, &plan.stable_addrs);

    // Report the user's tokens (+ native ETH) with both prices.
    let mut out = Vec::new();
    out.push(json!({
        "address": "ETH",
        "eth": 1.0,
        "usd": usd_prices.get(&plan.weth).copied(),
    }));
    for (spelling, addr) in &plan.requested {
        out.push(json!({
            "address": spelling,
            "eth": eth_prices.get(addr).copied(),
            "usd": usd_prices.get(addr).copied(),
        }));
    }
    json!({ "ok": true, "chainId": plan.chain_id, "prices": out }).to_string()
}

fn finish_quote(plan: &QuotePlan, results: &[Option<Vec<u8>>]) -> Result<swap::BestQuote, String> {
    swap::decode_best_quote(&plan.batch, results).ok_or_else(|| "no route found".to_string())
}

fn finish_quote_swap(plan: &QuotePlan, results: &[Option<Vec<u8>>]) -> String {
    match finish_quote(plan, results) {
        Ok(q) => json!({
            "ok": true,
            "version": format!("{:?}", q.version),
            "fee": q.fee,
            "amountOut": q.amount_out.to_string(),
        })
        .to_string(),
        Err(e) => err(e),
    }
}

fn finish_build_swap(plan: &BuildPlan, results: &[Option<Vec<u8>>]) -> String {
    let quote = match finish_quote(&plan.quote, results) {
        Ok(q) => q,
        Err(e) => return err(e),
    };

    // amountOutMin: explicit if given, else quote minus slippage.
    let amount_out_min = plan.amount_out_min.unwrap_or_else(|| {
        let bps = U256::from(10_000u64.saturating_sub(plan.slippage_bps));
        quote.amount_out.saturating_mul(bps) / U256::from(10_000u64)
    });

    let built = swap::build_swap(
        &plan.quote.chain,
        &quote,
        plan.token_in,
        plan.token_out,
        plan.amount_in,
        amount_out_min,
        plan.recipient,
        plan.deadline,
    );
    match built {
        Some(b) => {
            let approve = b.approve.map(|(token, spender, data)| {
                json!({ "token": format!("{token}"), "spender": format!("{spender}"), "data": format!("0x{}", hex::encode(data)) })
            });
            json!({
                "ok": true,
                "version": format!("{:?}", quote.version),
                "fee": quote.fee,
                "router": format!("{}", b.router),
                "value": format!("0x{:x}", b.value),
                "data": format!("0x{}", hex::encode(b.data)),
                "amountOut": quote.amount_out.to_string(),
                "amountOutMin": amount_out_min.to_string(),
                "approve": approve,
            })
            .to_string()
        }
        None => err("could not build swap for the best route (V4 swaps are a fast-follow)"),
    }
}

impl UniswapModuleImpl {
    /// Read the config under a shared lock (concurrent readers overlap). Clone out
    /// what you need and let the guard drop before the call goes out.
    fn with_cfg<R>(&self, f: impl FnOnce(&ConfigStore) -> R) -> Result<R, String> {
        match self.cfg.read().unwrap().as_ref() {
            Some(c) => Ok(f(c)),
            None => Err("uniswap not initialized (context not ready)".to_string()),
        }
    }

    /// Write the config (the only mutator is `configure`).
    fn with_cfg_mut(&self, f: impl FnOnce(&mut ConfigStore) -> bool) -> bool {
        match self.cfg.write().unwrap().as_mut() {
            Some(c) => f(c),
            None => false,
        }
    }

    /// Look up + clone a chain's config under the read lock.
    fn chain_cfg(&self, chain_id: i64) -> Result<ChainUniswap, String> {
        match self.with_cfg(|c| c.chain(chain_id as u64).cloned()) {
            Ok(Some(ch)) => Ok(ch),
            Ok(None) => Err(format!("no uniswap config for chain {chain_id}")),
            Err(e) => Err(e),
        }
    }

    /// Resolve chain config, WETH and the stablecoins (priced too, to anchor
    /// USD), and build the Multicall3 batch. No network, no lock held after it.
    fn plan_prices(&self, chain_id: i64, tokens_json: &str) -> Result<PricesPlan, String> {
        let req: PricesReq = serde_json::from_str(tokens_json).map_err(|e| e.to_string())?;
        let chain = self.chain_cfg(chain_id)?;
        let weth = parse_addr_opt(&chain.weth).ok_or("invalid WETH address in config")?;
        let stable_addrs: Vec<Address> = chain.stablecoins.iter().filter_map(|s| parse_addr_opt(s)).collect();

        // Price the user's tokens plus the stablecoins (USD anchor).
        let requested: Vec<(String, Address)> = req
            .tokens
            .iter()
            .filter_map(|t| parse_addr_opt(&t.address).map(|a| (t.address.clone(), a)))
            .collect();
        let mut priced: Vec<(Address, u8)> = Vec::new();
        for t in &req.tokens {
            if let Some(a) = parse_addr_opt(&t.address) {
                priced.push((a, t.decimals));
            }
        }
        for s in &stable_addrs {
            if !priced.iter().any(|(a, _)| a == s) {
                priced.push((*s, STABLE_DECIMALS));
            }
        }

        Ok(PricesPlan {
            chain_id,
            multicall3: chain.multicall3.clone(),
            weth,
            stable_addrs,
            requested,
            batch: pricing::build_pricing_batch(&chain, weth, &priced),
        })
    }

    /// Build the V2 + V3 quote batch for `amount_in` of `token_in → token_out`.
    fn plan_quote(&self, chain_id: i64, token_in: Address, token_out: Address, amount_in: U256) -> Result<QuotePlan, String> {
        let chain = self.chain_cfg(chain_id)?;
        let batch = swap::build_quote_batch(&chain, token_in, token_out, amount_in);
        Ok(QuotePlan { chain_id, chain, batch })
    }

    /// Parse a swap request and plan its quote. Shared by `quote_swap` and
    /// `build_swap`, which differ only in what they do with the reply.
    fn plan_swap_quote(&self, chain_id: i64, params_json: &str) -> Result<QuotePlan, String> {
        let p: SwapReq = serde_json::from_str(params_json).map_err(|e| e.to_string())?;
        let token_in = parse_token(&p.token_in)?;
        let token_out = parse_token(&p.token_out)?;
        self.plan_quote(chain_id, token_in, token_out, parse_u256(&p.amount_in))
    }

    fn plan_build(&self, chain_id: i64, params_json: &str) -> Result<BuildPlan, String> {
        let p: SwapReq = serde_json::from_str(params_json).map_err(|e| e.to_string())?;
        let token_in = parse_token(&p.token_in)?;
        let token_out = parse_token(&p.token_out)?;
        let amount_in = parse_u256(&p.amount_in);
        let recipient = parse_addr_opt(&p.recipient).ok_or("recipient required")?;
        let deadline = if p.deadline > 0 { p.deadline } else { now_secs() + 1200 };

        Ok(BuildPlan {
            quote: self.plan_quote(chain_id, token_in, token_out, amount_in)?,
            token_in,
            token_out,
            amount_in,
            amount_out_min: if p.amount_out_min.is_empty() { None } else { Some(parse_u256(&p.amount_out_min)) },
            slippage_bps: p.slippage_bps,
            recipient,
            deadline: U256::from(deadline),
        })
    }
}

/// Start `plan`'s call, park `finish(plan, reply)` on the board, and answer the
/// job id. The one place the three `start_*` methods differ is `finish`.
fn start_job<P, F>(chain_id: i64, request: Option<String>, plan: P, finish: F) -> String
where
    P: Send + 'static,
    F: FnOnce(&P, &[Option<Vec<u8>>]) -> String + Send + 'static,
{
    let job_id = JOBS.start();
    let job = job_id.clone();
    dispatch_multicall(chain_id, request, move |reply| {
        let answer = match reply {
            Ok(results) => finish(&plan, &results),
            Err(e) => err(e),
        };
        JOBS.complete(&job, answer);
    });
    json!({ "ok": true, "jobId": job_id }).to_string()
}

impl UniswapModule for UniswapModuleImpl {
    fn on_context_ready(&self, ctx: &RustModuleContext) {
        let dir = std::path::PathBuf::from(&ctx.instance_persistence_path);
        *self.cfg.write().unwrap() = Some(ConfigStore::with_path(dir.join("config.json")));
    }

    fn configure(&self, chain_json: String) -> bool {
        let chain: ChainUniswap = match serde_json::from_str(&chain_json) {
            Ok(c) => c,
            Err(_) => return false,
        };
        self.with_cfg_mut(|cfg| {
            cfg.set_chain(chain);
            true
        })
    }

    fn get_chains(&self) -> String {
        match self.with_cfg(|c| json!({ "ok": true, "chains": c.all() }).to_string()) {
            Ok(s) => s,
            Err(e) => err(e),
        }
    }

    fn get_prices(&self, chain_id: i64, tokens_json: String) -> String {
        let plan = match self.plan_prices(chain_id, &tokens_json) {
            Ok(p) => p,
            Err(e) => return err(e),
        };
        match await_multicall(plan.chain_id, multicall_request(&plan.multicall3, &plan.batch.calls)) {
            Ok(results) => finish_prices(&plan, &results),
            Err(e) => err(e),
        }
    }

    fn quote_swap(&self, chain_id: i64, params_json: String) -> String {
        let plan = match self.plan_swap_quote(chain_id, &params_json) {
            Ok(p) => p,
            Err(e) => return err(e),
        };
        match await_multicall(plan.chain_id, multicall_request(&plan.chain.multicall3, &plan.batch.calls)) {
            Ok(results) => finish_quote_swap(&plan, &results),
            Err(e) => err(e),
        }
    }

    fn build_swap(&self, chain_id: i64, params_json: String) -> String {
        let plan = match self.plan_build(chain_id, &params_json) {
            Ok(p) => p,
            Err(e) => return err(e),
        };
        let request = multicall_request(&plan.quote.chain.multicall3, &plan.quote.batch.calls);
        match await_multicall(plan.quote.chain_id, request) {
            Ok(results) => finish_build_swap(&plan, &results),
            Err(e) => err(e),
        }
    }

    fn start_get_prices(&self, chain_id: i64, tokens_json: String) -> String {
        let plan = match self.plan_prices(chain_id, &tokens_json) {
            Ok(p) => p,
            Err(e) => return err(e),
        };
        let request = multicall_request(&plan.multicall3, &plan.batch.calls);
        start_job(plan.chain_id, request, plan, finish_prices)
    }

    fn start_quote_swap(&self, chain_id: i64, params_json: String) -> String {
        let plan = match self.plan_swap_quote(chain_id, &params_json) {
            Ok(p) => p,
            Err(e) => return err(e),
        };
        let request = multicall_request(&plan.chain.multicall3, &plan.batch.calls);
        start_job(plan.chain_id, request, plan, finish_quote_swap)
    }

    fn start_build_swap(&self, chain_id: i64, params_json: String) -> String {
        let plan = match self.plan_build(chain_id, &params_json) {
            Ok(p) => p,
            Err(e) => return err(e),
        };
        let request = multicall_request(&plan.quote.chain.multicall3, &plan.quote.batch.calls);
        start_job(plan.quote.chain_id, request, plan, finish_build_swap)
    }

    fn take_result(&self, job_id: String) -> String {
        match JOBS.take(&job_id) {
            Job::Ready(answer) => answer,
            Job::Pending => json!({ "ok": false, "pending": true, "jobId": job_id }).to_string(),
            Job::Unknown => err(format!(
                "unknown job '{job_id}': never started, already collected, or evicted after {} newer ones",
                crate::jobs::CAPACITY
            )),
        }
    }
}

#[no_mangle]
pub extern "Rust" fn logos_module_install() {
    install::<UniswapModuleImpl>();
}
