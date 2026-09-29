//! Sequential OpenSea purchases with a USD price band and durable receipt tracking.
use crate::{
    arithmetic::{scale_u64, scale_u128},
    autosell::encode_fulfillment,
    config::{
        HYPEREVM_MAINNET_CHAIN_ID, INK_MAINNET_CHAIN_ID, OpenSeaExecutionMode, parse_native_amount,
        parse_usd_amount,
    },
    error::{BotError, Result},
    opensea::{OpenSeaClient, OpenSeaOfferFulfillment, opensea_chain_slug},
    pricing::{PriceOracle, format_usd},
    rpc::{RpcClients, simulate_call},
    wallet::{LoadedWallet, WalletNonceLock},
};
use alloy::{
    consensus::{Transaction, TxEnvelope},
    eips::{Decodable2718, Encodable2718},
    network::TransactionBuilder,
    primitives::{Address, B256, Bytes, U256, address, keccak256},
    rpc::types::{TransactionReceipt, TransactionRequest},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeSet,
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};

pub const SEAPORT: Address = address!("0000000000000068F116a894984e2DB1123eB395");

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AutoBuyConfig {
    pub chain_id: u64,
    pub contract_address: Address,
    pub target_price_usd: String,
    /// Symmetric percentage, with up to two decimal places (10 => +/- 10%).
    pub price_tolerance_percent: String,
    pub quantity: u64,
    #[serde(default)]
    pub gas_mode: OpenSeaExecutionMode,
    #[serde(default = "default_gas_cap")]
    pub max_gas_cost_native: String,
    /// Maximum cumulative gas lost to reverted transactions in this session.
    #[serde(default = "default_failed_gas_cap")]
    pub max_failed_gas_cost_native: String,
    #[serde(default = "default_poll")]
    pub poll_seconds: u64,
    #[serde(default = "default_receipt_timeout")]
    pub receipt_timeout_seconds: u64,
    #[serde(default = "default_confirmations")]
    pub confirmations: u64,
    /// Give a new session a new name to buy another batch with identical settings.
    #[serde(default = "default_session")]
    pub session: String,
}
fn default_gas_cap() -> String {
    "0.001".into()
}
fn default_failed_gas_cap() -> String {
    "0.003".into()
}
fn default_poll() -> u64 {
    5
}
fn default_receipt_timeout() -> u64 {
    180
}
fn default_confirmations() -> u64 {
    2
}
fn default_session() -> String {
    "default".into()
}

impl AutoBuyConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let config: Self = serde_json::from_slice(&std::fs::read(path)?)?;
        config.validate()?;
        Ok(config)
    }
    pub fn validate(&self) -> Result<()> {
        opensea_chain_slug(self.chain_id)?;
        if self.contract_address.is_zero()
            || self.quantity == 0
            || self.poll_seconds == 0
            || self.receipt_timeout_seconds == 0
            || self.confirmations == 0
            || self.session.trim().is_empty()
            || self.session.len() > 128
        {
            return Err(BotError::Config("auto-buy requires a nonzero contract, positive quantity/timers/confirmations, and a session name of 1–128 bytes".into()));
        }
        if parse_native_amount(&self.max_gas_cost_native)?.is_zero()
            || parse_native_amount(&self.max_failed_gas_cost_native)?.is_zero()
        {
            return Err(BotError::Config(
                "auto-buy gas budget must be positive".into(),
            ));
        }
        self.price_band()?;
        Ok(())
    }
    pub fn price_band(&self) -> Result<(U256, U256)> {
        let target = parse_usd_amount(&self.target_price_usd)?;
        if target.is_zero() {
            return Err(BotError::Config("target USD price must be positive".into()));
        }
        let percent = parse_usd_amount(&self.price_tolerance_percent)?;
        // Two decimal places of percent; retain exact integer comparisons.
        if percent > U256::from(100_000_000) || percent % U256::from(10_000) != U256::ZERO {
            return Err(BotError::Config(
                "price tolerance must be 0–100%, with at most two decimal places".into(),
            ));
        }
        let base = U256::from(100_000_000);
        let lower = target
            .checked_mul(base - percent)
            .ok_or_else(overflow)?
            .div_ceil(base);
        let upper = target.checked_mul(base + percent).ok_or_else(overflow)? / base;
        Ok((lower, upper))
    }
    pub fn in_price_band(&self, usd: U256) -> Result<bool> {
        let (lower, upper) = self.price_band()?;
        Ok(usd >= lower && usd <= upper)
    }
    pub fn native_symbol(&self) -> &'static str {
        if self.chain_id == HYPEREVM_MAINNET_CHAIN_ID {
            "HYPE"
        } else {
            "ETH"
        }
    }
    fn fee_multiplier(&self) -> f64 {
        if self.gas_mode == OpenSeaExecutionMode::Aggressive {
            2.0
        } else {
            1.2
        }
    }
}
fn overflow() -> BotError {
    BotError::Transaction("auto-buy amount overflow".into())
}
fn invalid(reason: &str) -> BotError {
    BotError::Transaction(format!("auto-buy: {reason}"))
}
fn field<'a>(v: &'a Value, key: &str) -> Result<&'a Value> {
    v.get(key).ok_or_else(|| invalid(&format!("missing {key}")))
}
fn uint(v: &Value) -> Result<U256> {
    match v {
        Value::String(s) => s.parse().map_err(|_| invalid("invalid integer")),
        Value::Number(n) => n
            .as_u64()
            .map(U256::from)
            .ok_or_else(|| invalid("invalid integer")),
        _ => Err(invalid("invalid integer")),
    }
}
fn addr(v: &Value) -> Result<Address> {
    v.as_str()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| invalid("invalid address"))
}
fn list(v: &Value) -> Result<&Vec<Value>> {
    v.as_array().ok_or_else(|| invalid("expected array"))
}

#[derive(Debug, Clone)]
pub struct Listing {
    pub hash: B256,
    pub token_id: U256,
    pub item_type: u8,
    pub value: U256,
}

/// Accept single-asset, fixed-price native listings only. Never trust currency
/// symbols to identify payment tokens: verify the signed consideration too.
pub fn parse_listing(v: &Value, config: &AutoBuyConfig, buyer: Address) -> Result<Listing> {
    if v.get("chain").and_then(Value::as_str) != Some(opensea_chain_slug(config.chain_id)?)
        || v.get("status").and_then(Value::as_str) != Some("ACTIVE")
        || uint(field(v, "remaining_quantity")?)?.is_zero()
        || addr(field(v, "protocol_address")?)? != SEAPORT
    {
        return Err(invalid(
            "listing is inactive or on the wrong chain/protocol",
        ));
    }
    let parameters = v
        .pointer("/protocol_data/parameters")
        .ok_or_else(|| invalid("missing listing order"))?;
    if addr(field(parameters, "offerer")?)? == buyer {
        return Err(invalid("own listing"));
    }
    let offers = list(field(parameters, "offer")?)?;
    if offers.len() != 1 {
        return Err(invalid("bundled listings are unsupported"));
    }
    let offer = &offers[0];
    let item_type = uint(field(offer, "itemType")?)?;
    if ![U256::from(2), U256::from(3)].contains(&item_type)
        || addr(field(offer, "token")?)? != config.contract_address
    {
        return Err(invalid("listing does not offer the selected NFT contract"));
    }
    let units = fixed_amount(offer)?;
    if units.is_zero() || (item_type == U256::from(2) && units != U256::from(1)) {
        return Err(invalid("invalid NFT units"));
    }
    let total = native_consideration(parameters, U256::from(1), units)?;
    let price = v
        .pointer("/price/current")
        .ok_or_else(|| invalid("missing listing price"))?;
    if uint(field(price, "decimals")?)? != U256::from(18)
        || price.get("currency").and_then(Value::as_str) != Some(config.native_symbol())
    {
        return Err(invalid(
            "listing is not priced in the chain's native currency",
        ));
    }
    // The fulfillment value is authoritative. Discovery uses signed per-unit
    // consideration because API current.price may represent all remaining units.
    let hash = field(v, "order_hash")?
        .as_str()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| invalid("invalid listing hash"))?;
    Ok(Listing {
        hash,
        token_id: uint(field(offer, "identifierOrCriteria")?)?,
        item_type: item_type.to::<u8>(),
        value: total,
    })
}
fn fixed_amount(item: &Value) -> Result<U256> {
    let start = uint(field(item, "startAmount")?)?;
    if start != uint(field(item, "endAmount")?)? {
        return Err(invalid("variable-price orders are unsupported"));
    }
    Ok(start)
}
fn native_consideration(parameters: &Value, numerator: U256, denominator: U256) -> Result<U256> {
    if denominator.is_zero() {
        return Err(invalid("zero order denominator"));
    }
    let items = list(field(parameters, "consideration")?)?;
    if items.is_empty() {
        return Err(invalid("empty consideration"));
    }
    let mut total = U256::ZERO;
    for item in items {
        if !uint(field(item, "itemType")?)?.is_zero()
            || !addr(field(item, "token")?)?.is_zero()
            || !uint(field(item, "identifierOrCriteria")?)?.is_zero()
        {
            return Err(invalid("only native-currency payment is supported"));
        }
        let amount = fixed_amount(item)?
            .checked_mul(numerator)
            .ok_or_else(overflow)?;
        // Seaport partial fills require exact divisibility of every item.
        if amount % denominator != U256::ZERO {
            return Err(invalid("inexact partial fill"));
        }
        total = total
            .checked_add(amount / denominator)
            .ok_or_else(overflow)?;
    }
    Ok(total)
}

/// Validate the very input encoded for signing, including contract, token, one
/// unit, native fees, recipient, chain and canonical Seaport function selector.
pub fn validate_fulfillment(
    fulfillment: &OpenSeaOfferFulfillment,
    selected: &Listing,
    config: &AutoBuyConfig,
    buyer: Address,
    now: u64,
) -> Result<Vec<u8>> {
    let tx = &fulfillment.transaction;
    if tx.chain != config.chain_id
        || tx.to != SEAPORT
        || !fulfillment.protocol.eq_ignore_ascii_case("seaport1.6")
    {
        return Err(invalid("unexpected fulfillment chain/protocol/target"));
    }
    let input = &tx.input_data;
    let name = crate::abi::parse_function(&tx.function)?.name;
    let parameters;
    let total = if name == "fulfillBasicOrder" || name == "fulfillBasicOrder_efficient_6GL6yc" {
        parameters = input
            .get("parameters")
            .or_else(|| input.get("basicOrderParameters"))
            .ok_or_else(|| invalid("missing basic order"))?;
        let route = uint(field(parameters, "basicOrderType")?)?;
        let expected_route = if selected.item_type == 2 { 0 } else { 4 };
        if route < U256::from(expected_route)
            || route >= U256::from(expected_route + 4)
            || !addr(field(parameters, "considerationToken")?)?.is_zero()
            || !uint(field(parameters, "considerationIdentifier")?)?.is_zero()
            || addr(field(parameters, "offerToken")?)? != config.contract_address
            || uint(field(parameters, "offerIdentifier")?)? != selected.token_id
            || uint(field(parameters, "offerAmount")?)? != U256::from(1)
        {
            return Err(invalid(
                "basic order changes the NFT, quantity, or payment asset",
            ));
        }
        let mut amount = uint(field(parameters, "considerationAmount")?)?;
        for recipient in list(field(parameters, "additionalRecipients")?)? {
            amount = amount
                .checked_add(uint(field(recipient, "amount")?)?)
                .ok_or_else(overflow)?;
        }
        amount
    } else if name == "fulfillOrder" || name == "fulfillAdvancedOrder" {
        let advanced = name == "fulfillAdvancedOrder";
        let order = if advanced {
            input
                .get("advancedOrder")
                .or_else(|| input.get("advanced_order"))
        } else {
            input.get("order")
        }
        .ok_or_else(|| invalid("missing order"))?;
        let nested = if advanced {
            order.get("order").unwrap_or(order)
        } else {
            order
        };
        parameters = nested.get("parameters").unwrap_or(nested);
        let (numerator, denominator) = if advanced {
            if input
                .get("criteriaResolvers")
                .or_else(|| input.get("criteria_resolvers"))
                .is_some_and(|v| v.as_array().is_none_or(|a| !a.is_empty()))
            {
                return Err(invalid("criteria orders are unsupported"));
            }
            (
                uint(field(order, "numerator")?)?,
                uint(field(order, "denominator")?)?,
            )
        } else {
            (U256::from(1), U256::from(1))
        };
        if numerator.is_zero() || numerator > denominator {
            return Err(invalid("invalid fill fraction"));
        }
        let offers = list(field(parameters, "offer")?)?;
        if offers.len() != 1 {
            return Err(invalid("bundled fulfillment"));
        }
        let offer = &offers[0];
        if addr(field(offer, "token")?)? != config.contract_address
            || uint(field(offer, "itemType")?)? != U256::from(selected.item_type)
            || uint(field(offer, "identifierOrCriteria")?)? != selected.token_id
            || fixed_amount(offer)?
                .checked_mul(numerator)
                .ok_or_else(overflow)?
                != denominator
        {
            return Err(invalid(
                "fulfillment must transfer exactly one selected NFT",
            ));
        }
        native_consideration(parameters, numerator, denominator)?
    } else {
        return Err(invalid("unsupported listing fulfillment function"));
    };
    if addr(field(parameters, "offerer")?)? == buyer
        || uint(field(parameters, "startTime")?)? > U256::from(now)
        || uint(field(parameters, "endTime")?)? <= U256::from(now)
    {
        return Err(invalid("order is self-owned, not started, or expired"));
    }
    if tx.value != total {
        return Err(invalid(
            "transaction value differs from the complete native payment including fees",
        ));
    }
    // Encoder validates the full canonical ABI and forces the advanced recipient
    // to the buyer. Regular/basic orders deliver to msg.sender.
    encode_fulfillment(fulfillment, buyer)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PendingBuy {
    hash: B256,
    order_hash: B256,
    token_id: U256,
    item_type: u8,
    #[serde(default)]
    raw_transaction: Option<Bytes>,
    #[serde(default)]
    nonce: Option<u64>,
    // Legacy journals may already have broadcast: never assume otherwise.
    #[serde(default = "already_attempted")]
    broadcast_attempted: bool,
}
fn already_attempted() -> bool {
    true
}

#[derive(Debug, Default)]
struct DiscoveryCursor {
    next: Option<String>,
    seen: BTreeSet<String>,
}
#[derive(Debug, Default, Serialize, Deserialize)]
struct BuyProgress {
    purchased: u64,
    completed_orders: BTreeSet<B256>,
    purchased_erc721: BTreeSet<U256>,
    pending: Option<PendingBuy>,
    #[serde(default)]
    failed_orders: BTreeSet<B256>,
    #[serde(default)]
    failed_gas_cost_native: U256,
    #[serde(skip)]
    discovery: DiscoveryCursor,
}
impl BuyProgress {
    fn load(path: &Path) -> Result<Self> {
        match std::fs::read(path) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }
    fn save(&self, path: &Path) -> Result<()> {
        let parent = path
            .parent()
            .ok_or_else(|| invalid("state path has no parent"))?;
        std::fs::create_dir_all(parent)?;
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        file.write_all(&serde_json::to_vec_pretty(self)?)?;
        file.as_file().sync_all()?;
        file.persist(path).map_err(|e| e.error)?;
        std::fs::File::open(parent)?.sync_all()?;
        Ok(())
    }
    fn finish(&mut self, success: bool) -> Result<()> {
        let pending = self
            .pending
            .take()
            .ok_or_else(|| invalid("missing pending purchase"))?;
        if success {
            self.purchased = self.purchased.checked_add(1).ok_or_else(overflow)?;
            // ERC-1155 listings can have more units; only deduplicate ERC-721.
            if pending.item_type == 2 {
                self.completed_orders.insert(pending.order_hash);
                self.purchased_erc721.insert(pending.token_id);
            }
        } else {
            self.failed_orders.insert(pending.order_hash);
        }
        Ok(())
    }
}
fn progress_path(config: &AutoBuyConfig, buyer: Address) -> PathBuf {
    // Bind identity to the wallet, collection, chain and explicit session, not
    // mutable prices/gas. Adjusting a threshold must not forget a pending tx.
    let identity = format!(
        "{}:{}:{}:{}",
        config.chain_id, config.contract_address, buyer, config.session
    );
    PathBuf::from("auto-buy-state").join(format!("{}.json", hex::encode(keccak256(identity))))
}

pub async fn run_auto_buy(config: AutoBuyConfig, dry_run: bool) -> Result<()> {
    config.validate()?;
    let (lower, upper) = config.price_band()?;
    println!(
        "Auto-buy: {} | {} | {} NFT(s)\nPrice range: {}–{} per NFT, including marketplace fees; gas separate.\nGas mode: {:?}; maximum {} {} per purchase.",
        opensea_chain_slug(config.chain_id)?,
        config.contract_address,
        config.quantity,
        format_usd(lower),
        format_usd(upper),
        config.gas_mode,
        config.max_gas_cost_native,
        config.native_symbol()
    );
    let wallet = LoadedWallet::from_env()?;
    let client = OpenSeaClient::from_env()?;
    let oracle = PriceOracle::new()?;
    println!(
        "Session gas-loss limit: {} {}. Reverted orders are skipped for this session.",
        config.max_failed_gas_cost_native,
        config.native_symbol()
    );
    let mut rpc = RpcClients::connect_from_env_for_chain(config.chain_id).await?;
    rpc.validate_chain_id(config.chain_id).await?;
    rpc.validate_contract_address(config.contract_address, None)
        .await?;
    rpc.validate_contract_address(SEAPORT, None).await?;
    let slug = client
        .collection_for_contract(config.chain_id, config.contract_address)
        .await?;
    println!("OpenSea collection: {slug}");
    // Exclusive session/wallet ownership also protects the progress journal.
    let _lock = tokio::select! {
        lock = WalletNonceLock::acquire(config.chain_id, wallet.address) => lock?,
        _ = tokio::signal::ctrl_c() => return Ok(()),
    };
    let path = progress_path(&config, wallet.address);
    BuyRunner {
        config: &config,
        client: &client,
        rpc: &rpc,
        oracle: &oracle,
        wallet: &wallet,
        slug: &slug,
        path: &path,
    }
    .run(dry_run)
    .await
}

struct BuyRunner<'a> {
    config: &'a AutoBuyConfig,
    client: &'a OpenSeaClient,
    rpc: &'a RpcClients,
    oracle: &'a PriceOracle,
    wallet: &'a LoadedWallet,
    slug: &'a str,
    path: &'a Path,
}
impl BuyRunner<'_> {
    async fn run(&self, dry_run: bool) -> Result<()> {
        let Self {
            config,
            client,
            rpc,
            oracle,
            wallet,
            slug,
            path,
        } = *self;
        let mut progress = BuyProgress::load(path)?;
        println!(
            "Progress: {}/{}; state: {}",
            progress.purchased,
            config.quantity,
            path.display()
        );
        if progress.pending.is_some() {
            if dry_run {
                return Err(invalid(
                    "a purchase is pending; run live mode to reconcile its receipt",
                ));
            }
            self.recover_pending(&mut progress).await?;
        }
        let mut poll = tokio::time::interval(Duration::from_secs(config.poll_seconds));
        poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        while progress.purchased < config.quantity {
            ensure_failure_budget(config, &progress, U256::ZERO)?;
            tokio::select! {
                _ = poll.tick() => {},
                _ = tokio::signal::ctrl_c() => return Ok(()),
            }
            // Ctrl-C may cancel discovery/preparation, never a submission: after
            // signing, persist the tx identity and retain it until receipt resolution.
            let prepared = tokio::select! {
                result = find_purchase(config, client, rpc, oracle, slug, wallet.address, &mut progress) => result,
                _ = tokio::signal::ctrl_c() => { println!("Auto-buy stopped; progress saved."); return Ok(()); }
            };
            match prepared {
                Ok(Some((listing, request))) => {
                    if dry_run {
                        println!(
                            "DRY RUN: purchase of token {} simulated successfully; no transaction signed or sent.",
                            listing.token_id
                        );
                        return Ok(());
                    }
                    let nonce = request.nonce;
                    let signed = wallet.sign_request(request).await?;
                    let raw = signed.encoded_2718();
                    let pending = PendingBuy {
                        hash: keccak256(&raw),
                        order_hash: listing.hash,
                        token_id: listing.token_id,
                        item_type: listing.item_type,
                        raw_transaction: Some(raw.into()),
                        nonce,
                        broadcast_attempted: false,
                    };
                    println!("Submitting token {}: {}", pending.token_id, pending.hash);
                    progress.pending = Some(pending);
                    progress.save(path)?;
                    self.recover_pending(&mut progress).await?;
                    println!(
                        "Purchased {}/{} NFT(s).",
                        progress.purchased, config.quantity
                    );
                    if progress.purchased >= config.quantity {
                        break;
                    }
                }
                Ok(None) => {
                    println!(
                        "Watching: no eligible native-currency listing in the price range ({}/{} bought).",
                        progress.purchased, config.quantity
                    );
                    if dry_run {
                        println!("DRY RUN: no matching purchase to simulate; no transaction sent.");
                        return Ok(());
                    }
                }
                Err(error) if retryable(&error) => {
                    tracing::warn!(%error, "auto-buy check failed; retrying without changing purchase count");
                    if dry_run {
                        return Err(error);
                    }
                }
                Err(error) => return Err(error),
            }
        }
        println!(
            "Auto-buy complete: {}/{} purchased. Use a new session name for another batch.",
            progress.purchased, config.quantity
        );
        Ok(())
    }

    async fn recover_pending(&self, progress: &mut BuyProgress) -> Result<()> {
        let pending = progress
            .pending
            .clone()
            .ok_or_else(|| invalid("missing pending purchase"))?;
        // A mined receipt (even if not yet fully confirmed) must be reconciled,
        // never replaced by another purchase or another nonce.
        if self.rpc.transaction_receipt(pending.hash).await?.is_none()
            && let Some(raw) = &pending.raw_transaction
        {
            validate_saved_transaction(&pending, self.config.chain_id, self.wallet.address)?;
            // Record ambiguity BEFORE calling RPC, covering a crash during send.
            progress
                .pending
                .as_mut()
                .expect("pending exists")
                .broadcast_attempted = true;
            progress.save(self.path)?;
            match self.rpc.broadcast_raw(raw.to_vec()).await {
                Ok(_) => {}
                Err(BotError::BroadcastRejected) if !pending.broadcast_attempted => {
                    // Only the first attempt can be proven not to have an older
                    // acceptance. On recovery, retain state despite rejection.
                    progress.finish(false)?;
                    progress.save(self.path)?;
                    tracing::warn!(hash = %pending.hash, "all endpoints rejected the first submission; skipping this order");
                    return Ok(());
                }
                Err(error) => {
                    tracing::warn!(%error, "retaining pending transaction and checking its receipt")
                }
            }
        }
        reconcile_pending(
            self.config,
            self.rpc,
            self.wallet.address,
            progress,
            self.path,
        )
        .await
    }
}

fn validate_saved_transaction(pending: &PendingBuy, chain_id: u64, buyer: Address) -> Result<()> {
    let raw = pending
        .raw_transaction
        .as_ref()
        .ok_or_else(|| invalid("missing saved transaction bytes"))?;
    let mut input = raw.as_ref();
    let tx =
        TxEnvelope::decode_2718(&mut input).map_err(|_| invalid("invalid saved transaction"))?;
    if !input.is_empty()
        || keccak256(raw) != pending.hash
        || tx.chain_id() != Some(chain_id)
        || Some(tx.nonce()) != pending.nonce
        || tx.to() != Some(SEAPORT)
        || tx
            .signature()
            .recover_address_from_prehash(&tx.signature_hash())
            .ok()
            != Some(buyer)
    {
        return Err(invalid(
            "saved transaction does not match its hash, nonce, chain, target, or wallet",
        ));
    }
    Ok(())
}

fn ensure_failure_budget(
    config: &AutoBuyConfig,
    progress: &BuyProgress,
    reserve: U256,
) -> Result<()> {
    let cap = parse_native_amount(&config.max_failed_gas_cost_native)?;
    if progress.failed_gas_cost_native >= cap
        || progress
            .failed_gas_cost_native
            .checked_add(reserve)
            .ok_or_else(overflow)?
            > cap
    {
        return Err(invalid(
            "session gas-loss budget exhausted or insufficient for another attempt; review max_failed_gas_cost_native",
        ));
    }
    Ok(())
}

fn retryable(error: &BotError) -> bool {
    matches!(
        error,
        BotError::OpenSeaTransport
            | BotError::PriceUnavailable(_)
            | BotError::Rpc(_)
            | BotError::OpenSeaApi {
                status: 408 | 429 | 500..=599,
                ..
            }
    )
}

async fn find_purchase(
    config: &AutoBuyConfig,
    client: &OpenSeaClient,
    rpc: &RpcClients,
    oracle: &PriceOracle,
    slug: &str,
    buyer: Address,
    progress: &mut BuyProgress,
) -> Result<Option<(Listing, TransactionRequest)>> {
    // Every cycle refreshes the floor, then at most one later page. Preserve
    // that later-page cursor so large collections still get complete coverage.
    for page_index in 0..2 {
        let next = if page_index == 0 {
            None
        } else {
            progress.discovery.next.clone()
        };
        if page_index == 1 && next.is_none() {
            break;
        }
        let page = client.listing_page(slug, next.as_deref()).await?;
        let following = page
            .get("next")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        if page_index == 0 && progress.discovery.next.is_none() {
            progress.discovery.seen.clear();
            progress.discovery.next = following;
        } else if page_index == 1 {
            if !progress
                .discovery
                .seen
                .insert(next.expect("later page has cursor"))
            {
                progress.discovery = DiscoveryCursor::default();
                tracing::warn!("OpenSea repeated a cursor; restarting discovery from the floor");
                break;
            }
            progress.discovery.next = following;
        }
        let snapshot = oracle
            .snapshot(&[config.native_symbol()], &Default::default())
            .await?;
        let listings = list(field(&page, "listings")?)?;
        for value in listings {
            let listing = match parse_listing(value, config, buyer) {
                Ok(listing) => listing,
                Err(error) => {
                    tracing::debug!(%error, "skipping unsupported listing");
                    continue;
                }
            };
            if progress.completed_orders.contains(&listing.hash)
                || progress.failed_orders.contains(&listing.hash)
                || (listing.item_type == 2 && progress.purchased_erc721.contains(&listing.token_id))
            {
                continue;
            }
            let usd = oracle.cost_to_usd(&snapshot, config.native_symbol(), listing.value, 18)?;
            if !config.in_price_band(usd)? {
                continue;
            }
            let fulfillment = match client
                .build_listing_fulfillment(listing.hash, config.chain_id, buyer)
                .await
            {
                Ok(f) => f,
                Err(BotError::OpenSeaApi {
                    status: 400 | 404 | 409,
                    ..
                }) => continue,
                Err(error) => return Err(error),
            };
            let calldata = match validate_fulfillment(
                &fulfillment,
                &listing,
                config,
                buyer,
                rpc.latest_timestamp().await?,
            ) {
                Ok(data) => data,
                Err(error) => {
                    tracing::warn!(%error, "skipping unsafe or unavailable fulfillment");
                    continue;
                }
            };
            let mut request = TransactionRequest::default()
                .with_from(buyer)
                .with_to(SEAPORT)
                .with_chain_id(config.chain_id)
                .with_value(fulfillment.transaction.value)
                .with_input(calldata);
            let fees = rpc.estimate_eip1559_fees().await?;
            request.set_max_fee_per_gas(scale_u128(fees.max_fee_per_gas, config.fee_multiplier())?);
            request.set_max_priority_fee_per_gas(scale_u128(
                fees.max_priority_fee_per_gas,
                config.fee_multiplier(),
            )?);
            // Always estimate the actual Seaport call, including Abstract pubdata.
            let gas = match rpc.estimate_gas(request.clone()).await {
                Ok(gas) => scale_u64(gas, 1.2)?,
                Err(error) => {
                    tracing::warn!(%error, "listing simulation/estimate failed; continuing");
                    continue;
                }
            };
            request.set_gas_limit(gas);
            let mut gas_cost = U256::from(gas)
                .checked_mul(U256::from(request.max_fee_per_gas.unwrap_or_default()))
                .ok_or_else(overflow)?;
            if config.chain_id == INK_MAINNET_CHAIN_ID {
                gas_cost = gas_cost
                    .checked_add(
                        rpc.ink_extra_fee_reserve(
                            request.input.input.as_ref().map_or(0, |b| b.len()),
                            gas,
                        )
                        .await?,
                    )
                    .ok_or_else(overflow)?;
            }
            let cap = parse_native_amount(&config.max_gas_cost_native)?;
            if gas_cost > cap {
                tracing::warn!(%gas_cost, %cap, "purchase gas exceeds budget; waiting");
                continue;
            }
            ensure_failure_budget(config, progress, gas_cost)?;
            let needed = fulfillment
                .transaction
                .value
                .checked_add(gas_cost)
                .ok_or_else(overflow)?;
            let balance = rpc.check_balance(buyer).await?;
            if needed > balance {
                return Err(BotError::InsufficientBalance {
                    needed: needed.to_string(),
                    available: balance.to_string(),
                });
            }
            request.set_nonce(rpc.preload_nonce(buyer).await?);
            if let Err(error) = simulate_call(rpc, request.clone()).await {
                tracing::warn!(%error, "listing became unavailable; continuing");
                continue;
            }
            // Refresh the USD quote after HTTP/RPC preparation, before signing.
            let fresh = oracle
                .snapshot(&[config.native_symbol()], &Default::default())
                .await?;
            let usd = oracle.cost_to_usd(
                &fresh,
                config.native_symbol(),
                fulfillment.transaction.value,
                18,
            )?;
            if !config.in_price_band(usd)? {
                continue;
            }
            println!(
                "Matching token {}: {} including marketplace fees.",
                listing.token_id,
                format_usd(usd)
            );
            return Ok(Some((listing, request)));
        }
    }
    Ok(None)
}

async fn reconcile_pending(
    config: &AutoBuyConfig,
    rpc: &RpcClients,
    buyer: Address,
    progress: &mut BuyProgress,
    path: &Path,
) -> Result<()> {
    let pending = progress
        .pending
        .clone()
        .ok_or_else(|| invalid("missing pending transaction"))?;
    let monitor = async {
        loop {
            if let Ok(Some(receipt)) = rpc.transaction_receipt(pending.hash).await
                && let (Some(block), Some(hash)) = (receipt.block_number, receipt.block_hash)
                && let Ok(head) = rpc.block_number().await
                && head >= block.saturating_add(config.confirmations - 1)
                && let Ok(Some(canonical)) = rpc.transaction_receipt(pending.hash).await
                && canonical.block_hash == Some(hash)
                && canonical.block_number == Some(block)
            {
                return canonical;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    };
    let receipt = tokio::select! {
        result = tokio::time::timeout(Duration::from_secs(config.receipt_timeout_seconds), monitor) =>
            result.map_err(|_| BotError::BroadcastOutcomeUnknown { hash: pending.hash })?,
        _ = tokio::signal::ctrl_c() => return Err(BotError::BroadcastOutcomeUnknown { hash: pending.hash }),
    };
    if receipt.status() && !received_nft(&receipt, config.contract_address, buyer, &pending) {
        return Err(invalid(
            "successful receipt did not prove receipt of the selected NFT; pending state retained for inspection",
        ));
    }
    if !receipt.status() {
        let mut cost = U256::from(receipt.gas_used)
            .checked_mul(U256::from(receipt.effective_gas_price))
            .ok_or_else(overflow)?;
        if config.chain_id == INK_MAINNET_CHAIN_ID {
            cost = cost
                .checked_add(rpc.ink_receipt_extra_fee(&receipt).await?)
                .ok_or_else(overflow)?;
        }
        progress.failed_gas_cost_native = progress
            .failed_gas_cost_native
            .checked_add(cost)
            .ok_or_else(overflow)?;
        println!(
            "Purchase reverted; skipping this order for the rest of the session. Watching other listings."
        );
    }
    progress.finish(receipt.status())?;
    progress.save(path)
}
fn received_nft(
    receipt: &TransactionReceipt,
    contract: Address,
    buyer: Address,
    pending: &PendingBuy,
) -> bool {
    receipt.inner.logs().iter().any(|log| {
        if log.address() != contract {
            return false;
        }
        let topics = log.topics();
        let buyer_topic = B256::from(buyer.into_word());
        if pending.item_type == 2 {
            topics.len() == 4
                && topics[0] == keccak256("Transfer(address,address,uint256)")
                && topics[1] != buyer_topic
                && topics[2] == buyer_topic
                && U256::from_be_slice(topics[3].as_slice()) == pending.token_id
        } else {
            let data = &log.data().data;
            topics.len() == 4
                && topics[0] == keccak256("TransferSingle(address,address,address,uint256,uint256)")
                && topics[2] != buyer_topic
                && topics[3] == buyer_topic
                && data.len() == 64
                && U256::from_be_slice(&data[..32]) == pending.token_id
                && U256::from_be_slice(&data[32..]) == U256::from(1)
        }
    })
}

#[cfg(test)]
#[path = "autobuy_tests.rs"]
mod tests;
