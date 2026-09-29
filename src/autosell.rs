use crate::{
    abi::parse_function,
    arithmetic::{scale_u64, scale_u128},
    config::{GasMode, MintConfig, parse_gwei, parse_native_amount, parse_usd_amount},
    error::{BotError, Result},
    opensea::{OpenSeaClient, OpenSeaOffer, OpenSeaOfferFulfillment},
    pricing::{PriceOracle, PriceSnapshot, format_usd},
    rpc::RpcClients,
    wallet::{LoadedWallet, WalletNonceLock},
};
use alloy::{
    dyn_abi::{DynSolType, DynSolValue, FunctionExt, JsonAbiExt, Specifier},
    eips::{BlockId, Encodable2718},
    network::TransactionBuilder,
    primitives::{Address, B256, U256, keccak256},
    rpc::types::{TransactionReceipt, TransactionRequest},
};
use serde_json::{Map, Value};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profitability {
    pub mint_payment_usd: U256,
    pub mint_gas_usd: U256,
    pub gross_offer_usd: U256,
    pub fee_usd: U256,
    pub sell_gas_usd: U256,
    pub approval_gas_usd: U256,
    pub total_cost_usd: U256,
    pub net_offer_usd: U256,
    pub profit_usd: U256,
    pub loss_usd: U256,
    pub profitable: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FulfillmentPayout {
    pub gross_amount: U256,
    pub fee_amount: U256,
    pub seller_amount: U256,
}

#[derive(Debug, Clone)]
pub struct AutoSellSummary {
    pub token_ids: Vec<U256>,
    pub sold: Vec<U256>,
    pub skipped: Vec<U256>,
}

#[derive(Debug, Clone, Copy)]
pub struct CostBasis {
    pub mint_payment_usd: U256,
    pub mint_gas_usd: U256,
}

#[derive(Debug, Clone)]
struct RawCostBasis {
    mint_payment_amount: U256,
    mint_payment_currency: String,
    mint_payment_decimals: u8,
    mint_gas_amount: U256,
    native_currency: String,
}

/// Run the post-mint strategy for every token emitted to the configured
/// wallet by the successful mint receipt. The method intentionally skips a
/// token when OpenSea has not indexed it, has no active offer, or the USD
/// calculation is not safe to make.
pub async fn run_after_mint(
    config: &MintConfig,
    rpc: &RpcClients,
    wallet: &LoadedWallet,
    client: &OpenSeaClient,
    receipt: &TransactionReceipt,
    mint_payment: U256,
) -> Result<AutoSellSummary> {
    if !config.auto_sell.enabled {
        return Ok(AutoSellSummary {
            token_ids: Vec::new(),
            sold: Vec::new(),
            skipped: Vec::new(),
        });
    }
    let contract = config.contract()?;
    let token_ids = minted_token_ids(receipt, contract, wallet.address);
    if token_ids.is_empty() {
        return Err(BotError::Transaction(
            "mint succeeded but no ERC-721/ERC-1155 token IDs were found in its receipt"
                .to_string(),
        ));
    }
    let slug = config
        .auto_sell
        .collection_slug
        .as_deref()
        .ok_or_else(|| BotError::Config("auto_sell.collection_slug is required".to_string()))?;
    let oracle = PriceOracle::new()?;
    let mut total_cost_basis = raw_mint_cost_basis(config, mint_payment, receipt)?;
    if config.chain_id == crate::config::INK_MAINNET_CHAIN_ID {
        total_cost_basis.mint_gas_amount = total_cost_basis
            .mint_gas_amount
            .checked_add(rpc.ink_receipt_extra_fee(receipt).await?)
            .ok_or_else(|| BotError::Transaction("mint fee overflowed".into()))?;
    }
    // A quantity mint pays one transaction-level cost. Allocate that cost
    // across the emitted token IDs so a batch mint can sell each token
    // independently without requiring every token to cover the whole batch.
    let token_count = U256::from(token_ids.len());
    let cost_basis = RawCostBasis {
        mint_payment_amount: total_cost_basis.mint_payment_amount.div_ceil(token_count),
        mint_payment_currency: total_cost_basis.mint_payment_currency,
        mint_payment_decimals: total_cost_basis.mint_payment_decimals,
        mint_gas_amount: total_cost_basis.mint_gas_amount.div_ceil(token_count),
        native_currency: total_cost_basis.native_currency,
    };
    println!("\nAUTO-SELL: {} token(s) detected", token_ids.len());

    let mut sold = Vec::new();
    let mut skipped = Vec::new();
    for token_id in token_ids.iter().copied() {
        match sell_one(
            config,
            rpc,
            wallet,
            client,
            &oracle,
            &cost_basis,
            slug,
            contract,
            token_id,
        )
        .await
        {
            Ok(true) => sold.push(token_id),
            Ok(false) => skipped.push(token_id),
            Err(error @ BotError::BroadcastOutcomeUnknown { .. }) => return Err(error),
            Err(error) if !config.auto_sell.require_usd_price => {
                tracing::warn!(token_id = %token_id, error = %error, "auto-sell skipped token");
                skipped.push(token_id);
            }
            Err(error) => return Err(error),
        }
    }
    Ok(AutoSellSummary {
        token_ids,
        sold,
        skipped,
    })
}

/// Exercise offer lookup, exact fee extraction, approvals, gas estimation, USD
/// conversion, and profitability for an existing token without signing or
/// broadcasting anything. This is the auto-sell half of `simulate`/dry-run.
pub async fn simulate_auto_sell(
    config: &MintConfig,
    rpc: &RpcClients,
    wallet: &LoadedWallet,
    client: &OpenSeaClient,
    token_id: U256,
    mint_payment: U256,
    mint_gas_amount: U256,
) -> Result<bool> {
    let contract = config.contract()?;
    let slug = config
        .auto_sell
        .collection_slug
        .as_deref()
        .ok_or_else(|| BotError::Config("auto_sell.collection_slug is required".to_string()))?;
    let offer = client
        .get_best_offer(slug, token_id)
        .await?
        .ok_or_else(|| BotError::Transaction("dry-run token has no active offer".to_string()))?;
    if !offer.status.eq_ignore_ascii_case("ACTIVE") || offer.remaining_quantity == 0 {
        return Err(BotError::Transaction(
            "dry-run token's best offer is not active".to_string(),
        ));
    }
    validate_offer(&offer, config, contract, token_id)?;
    if !wallet_owns_token(rpc, contract, wallet.address, token_id).await? {
        return Err(BotError::Transaction(
            "dry-run token is not owned by the configured wallet".to_string(),
        ));
    }
    let fulfillment = client
        .build_offer_fulfillment(
            &offer,
            wallet.address,
            config.auto_sell.include_optional_creator_fees,
        )
        .await?;
    validate_fulfillment_policy(config, &fulfillment, &offer, wallet.address)?;
    validate_offer_decimals(config, rpc, &offer).await?;
    let payout = fulfillment_payout(&fulfillment, &offer, wallet.address)?;
    let calldata = encode_fulfillment(&fulfillment, wallet.address)?;
    let operator = fulfillment.transaction.to;
    let approved =
        is_approved_for_all(rpc, offer.contract_address, wallet.address, operator).await?;
    let mut sell_request = TransactionRequest::default()
        .with_from(wallet.address)
        .with_to(operator)
        .with_chain_id(config.chain_id)
        .with_input(calldata)
        .with_value(fulfillment.transaction.value);
    apply_sell_fee_fields(config, rpc, &mut sell_request).await?;
    let sell_gas = if approved {
        scale_u64(
            rpc.estimate_gas(sell_request.clone()).await?,
            config.gas.multiplier,
        )?
    } else {
        config.gas.gas_limit.unwrap_or(350_000).max(350_000)
    };
    sell_request.set_gas_limit(sell_gas);
    let approval_request = if approved {
        None
    } else {
        Some(
            build_approval_request(
                config,
                rpc,
                wallet.address,
                offer.contract_address,
                operator,
                None,
            )
            .await?,
        )
    };
    let quantity = U256::from(config.quantity);
    let payment_currency = config
        .mint_payment_currency
        .as_deref()
        .unwrap_or_else(|| config.native_currency_symbol());
    let raw_cost_basis = RawCostBasis {
        mint_payment_amount: mint_payment.div_ceil(quantity),
        mint_payment_currency: payment_currency.to_string(),
        mint_payment_decimals: config.mint_payment_decimals,
        mint_gas_amount: mint_gas_amount.div_ceil(quantity),
        native_currency: config.native_currency_symbol().to_string(),
    };
    let oracle = PriceOracle::new()?;
    let prices = fresh_prices(config, &oracle, &raw_cost_basis, &offer.currency).await?;
    let cost_basis = usd_cost_basis(&oracle, &prices, &raw_cost_basis)?;
    let sell_gas_cost = transaction_gas_budget(config, rpc, &sell_request).await?;
    let approval_gas_cost = match approval_request.as_ref() {
        Some(request) => transaction_gas_budget(config, rpc, request).await?,
        None => U256::ZERO,
    };
    let profitability = calculate_profitability(
        &oracle,
        &prices,
        config,
        cost_basis,
        &offer,
        payout,
        sell_gas_cost,
        approval_gas_cost,
    )?;
    if let Some(cap) = config.auto_sell.max_sell_gas_cost_native.as_deref()
        && sell_gas_cost > parse_native_amount(cap)?
    {
        return Err(BotError::Transaction(
            "dry-run sell gas exceeds auto_sell.max_sell_gas_cost_native".to_string(),
        ));
    }
    println!("\nAUTO-SELL DRY-RUN");
    println!("Token: {token_id}");
    println!("Offer: {} {}", offer.value, offer.currency);
    println!("Encoded fee: {}", format_usd(profitability.fee_usd));
    println!(
        "Approval: {}",
        if approved {
            "already granted"
        } else {
            "required and gas-estimated"
        }
    );
    println!("Net after gas: {}", format_usd(profitability.net_offer_usd));
    println!("Result: {}", format_profit(&profitability));
    println!(
        "Decision: {} (no transaction submitted)",
        if profitability.profitable {
            "would sell"
        } else {
            "would skip"
        }
    );
    Ok(profitability.profitable)
}

fn raw_mint_cost_basis(
    config: &MintConfig,
    mint_payment: U256,
    receipt: &TransactionReceipt,
) -> Result<RawCostBasis> {
    let payment_currency = config
        .mint_payment_currency
        .as_deref()
        .unwrap_or_else(|| config.native_currency_symbol());
    // The transaction value is authoritative, including a freshly priced OpenSea mint.
    let payment_amount = mint_payment;
    let native_currency = config.native_currency_symbol();
    let mint_gas = U256::from(receipt.gas_used)
        .checked_mul(U256::from(receipt.effective_gas_price))
        .ok_or_else(|| BotError::Transaction("mint gas amount overflowed".to_string()))?;
    Ok(RawCostBasis {
        mint_payment_amount: payment_amount,
        mint_payment_currency: payment_currency.to_string(),
        mint_payment_decimals: config.mint_payment_decimals,
        mint_gas_amount: mint_gas,
        native_currency: native_currency.to_string(),
    })
}

fn usd_cost_basis(
    oracle: &PriceOracle,
    prices: &PriceSnapshot,
    raw: &RawCostBasis,
) -> Result<CostBasis> {
    Ok(CostBasis {
        mint_payment_usd: oracle.cost_to_usd(
            prices,
            &raw.mint_payment_currency,
            raw.mint_payment_amount,
            raw.mint_payment_decimals,
        )?,
        mint_gas_usd: oracle.cost_to_usd(prices, &raw.native_currency, raw.mint_gas_amount, 18)?,
    })
}

#[allow(clippy::too_many_arguments)]
async fn sell_one(
    config: &MintConfig,
    rpc: &RpcClients,
    wallet: &LoadedWallet,
    client: &OpenSeaClient,
    oracle: &PriceOracle,
    cost_basis: &RawCostBasis,
    slug: &str,
    contract: Address,
    token_id: U256,
) -> Result<bool> {
    let deadline = Instant::now()
        .checked_add(Duration::from_secs(config.auto_sell.offer_wait_seconds))
        .unwrap_or_else(Instant::now);
    let offer = loop {
        if let Some(offer) = client.get_best_offer(slug, token_id).await?
            && offer.status.eq_ignore_ascii_case("ACTIVE")
            && offer.remaining_quantity > 0
        {
            break offer;
        }
        if Instant::now() >= deadline {
            println!("Token {token_id}: no active OpenSea offer before timeout; skipped");
            return Ok(false);
        }
        tokio::time::sleep(Duration::from_secs(config.auto_sell.offer_poll_seconds)).await;
    };
    validate_offer(&offer, config, contract, token_id)?;
    evaluate_and_maybe_sell(config, rpc, wallet, client, oracle, cost_basis, offer).await
}

fn validate_offer(
    offer: &OpenSeaOffer,
    config: &MintConfig,
    contract: Address,
    token_id: U256,
) -> Result<()> {
    if offer.contract_address != contract || offer.token_id != token_id {
        return Err(BotError::Transaction(
            "OpenSea returned an offer for a different NFT".to_string(),
        ));
    }
    if let Some(expected_chain) = expected_chain_slug(config)
        && !offer.chain.eq_ignore_ascii_case(expected_chain)
    {
        return Err(BotError::Transaction(format!(
            "OpenSea offer is on `{}`, expected `{expected_chain}` for chain ID {}",
            offer.chain, config.chain_id
        )));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn evaluate_and_maybe_sell(
    config: &MintConfig,
    rpc: &RpcClients,
    wallet: &LoadedWallet,
    client: &OpenSeaClient,
    oracle: &PriceOracle,
    raw_cost_basis: &RawCostBasis,
    offer: OpenSeaOffer,
) -> Result<bool> {
    let fulfillment = client
        .build_offer_fulfillment(
            &offer,
            wallet.address,
            config.auto_sell.include_optional_creator_fees,
        )
        .await?;
    validate_fulfillment_policy(config, &fulfillment, &offer, wallet.address)?;
    validate_offer_decimals(config, rpc, &offer).await?;
    let payout = fulfillment_payout(&fulfillment, &offer, wallet.address)?;
    let calldata = encode_fulfillment(&fulfillment, wallet.address)?;
    let operator = fulfillment.transaction.to;
    let mut sell_request = TransactionRequest::default()
        .with_from(wallet.address)
        .with_to(fulfillment.transaction.to)
        .with_chain_id(config.chain_id)
        .with_input(calldata)
        .with_value(fulfillment.transaction.value);
    apply_sell_fee_fields(config, rpc, &mut sell_request).await?;
    let operator_approved =
        is_approved_for_all(rpc, offer.contract_address, wallet.address, operator).await?;
    // The sale cannot be simulated before Seaport is approved, but the
    // approval itself can and must be estimated exactly before deciding.
    let estimated_sell_gas = if operator_approved {
        scale_u64(
            rpc.estimate_gas(sell_request.clone()).await?,
            config.gas.multiplier,
        )?
    } else {
        config.gas.gas_limit.unwrap_or(350_000).max(350_000)
    };
    sell_request.set_gas_limit(estimated_sell_gas);
    let mut approval_request = if operator_approved {
        None
    } else {
        Some(
            build_approval_request(
                config,
                rpc,
                wallet.address,
                offer.contract_address,
                operator,
                None,
            )
            .await?,
        )
    };
    let prices = fresh_prices(config, oracle, raw_cost_basis, &offer.currency).await?;
    let cost_basis = usd_cost_basis(oracle, &prices, raw_cost_basis)?;
    let sell_gas_cost = transaction_gas_budget(config, rpc, &sell_request).await?;
    let approval_gas_cost = match approval_request.as_ref() {
        Some(request) => transaction_gas_budget(config, rpc, request).await?,
        None => U256::ZERO,
    };
    let profitability = calculate_profitability(
        oracle,
        &prices,
        config,
        cost_basis,
        &offer,
        payout,
        sell_gas_cost,
        approval_gas_cost,
    )?;
    println!(
        "Token {}: offer {} {} => seller payout {}, net after gas {}, {}",
        offer.token_id,
        offer.value,
        offer.currency,
        format_usd(profitability.gross_offer_usd - profitability.fee_usd),
        format_usd(profitability.net_offer_usd),
        format_profit(&profitability)
    );
    if !profitability.profitable {
        println!(
            "Token {}: offer below configured USD profitability threshold; skipped",
            offer.token_id
        );
        return Ok(false);
    }
    if let Some(cap) = config.auto_sell.max_sell_gas_cost_native.as_deref() {
        let cap = parse_native_amount(cap)?;
        if sell_gas_cost > cap {
            println!(
                "Token {}: sell gas exceeds configured cap; skipped",
                offer.token_id
            );
            return Ok(false);
        }
    }

    // The offer may be consumed while an approval transaction is mined. The
    // final offer lookup and fulfillment build happen immediately before the
    // sale transaction, after approval if one was needed.
    let _nonce_lock = WalletNonceLock::acquire(config.chain_id, wallet.address).await?;
    let mut nonce = rpc.preload_nonce(wallet.address).await?;
    let mut actual_approval_cost = U256::ZERO;
    if let Some(mut approval) = approval_request.take() {
        approval.set_nonce(nonce);
        let approval_hash = send_request(config, rpc, wallet, approval).await?;
        let approval_receipt = wait_for_receipt(config, rpc, approval_hash).await?;
        if !approval_receipt.status() {
            return Err(BotError::Transaction(
                "NFT approval transaction reverted; offer was not accepted".to_string(),
            ));
        }
        actual_approval_cost = receipt_gas_cost(&approval_receipt)?;
        if config.chain_id == crate::config::INK_MAINNET_CHAIN_ID {
            actual_approval_cost = actual_approval_cost
                .checked_add(rpc.ink_receipt_extra_fee(&approval_receipt).await?)
                .ok_or_else(|| BotError::Transaction("approval fee overflowed".into()))?;
        }
        println!("Token {}: NFT approval confirmed", offer.token_id);
        nonce = rpc.preload_nonce(wallet.address).await?;
    }

    let latest_offer = client
        .get_best_offer(
            config
                .auto_sell
                .collection_slug
                .as_deref()
                .unwrap_or_default(),
            offer.token_id,
        )
        .await?
        .ok_or_else(|| BotError::Transaction("best offer disappeared before sale".to_string()))?;
    if !latest_offer.status.eq_ignore_ascii_case("ACTIVE") || latest_offer.remaining_quantity == 0 {
        println!(
            "Token {}: best offer is no longer active before submission; skipped",
            offer.token_id
        );
        return Ok(false);
    }
    validate_offer(
        &latest_offer,
        config,
        offer.contract_address,
        offer.token_id,
    )?;
    let latest_fulfillment = client
        .build_offer_fulfillment(
            &latest_offer,
            wallet.address,
            config.auto_sell.include_optional_creator_fees,
        )
        .await?;
    validate_fulfillment_policy(config, &latest_fulfillment, &latest_offer, wallet.address)?;
    validate_offer_decimals(config, rpc, &latest_offer).await?;
    let latest_payout = fulfillment_payout(&latest_fulfillment, &latest_offer, wallet.address)?;
    let latest_calldata = encode_fulfillment(&latest_fulfillment, wallet.address)?;
    if latest_fulfillment.transaction.to != operator {
        return Err(BotError::Transaction(
            "OpenSea changed the Seaport fulfillment target before submission".to_string(),
        ));
    }
    let mut latest_request = TransactionRequest::default()
        .with_from(wallet.address)
        .with_to(latest_fulfillment.transaction.to)
        .with_chain_id(config.chain_id)
        .with_input(latest_calldata)
        .with_value(latest_fulfillment.transaction.value);
    apply_sell_fee_fields(config, rpc, &mut latest_request).await?;
    latest_request.set_nonce(nonce);
    let latest_gas = scale_u64(
        rpc.estimate_gas(latest_request.clone()).await?,
        config.gas.multiplier,
    )?;
    latest_request.set_gas_limit(latest_gas);
    let latest_gas_cost = transaction_gas_budget(config, rpc, &latest_request).await?;
    if let Some(cap) = config.auto_sell.max_sell_gas_cost_native.as_deref() {
        let cap = parse_native_amount(cap)?;
        if latest_gas_cost > cap {
            println!(
                "Token {}: updated sell gas exceeds configured cap; skipped",
                offer.token_id
            );
            return Ok(false);
        }
    }
    // Refresh every relevant currency immediately before the irreversible
    // broadcast. This also revalues the original mint cost at the same instant
    // as the proceeds and gas costs.
    let latest_prices =
        fresh_prices(config, oracle, raw_cost_basis, &latest_offer.currency).await?;
    let latest_cost_basis = usd_cost_basis(oracle, &latest_prices, raw_cost_basis)?;
    let latest_check = calculate_profitability(
        oracle,
        &latest_prices,
        config,
        latest_cost_basis,
        &latest_offer,
        latest_payout,
        latest_gas_cost,
        actual_approval_cost,
    )?;
    if !latest_check.profitable {
        println!(
            "Token {}: offer changed below threshold before submission; skipped",
            offer.token_id
        );
        return Ok(false);
    }
    let sale_hash = send_request(config, rpc, wallet, latest_request).await?;
    let sale_receipt = wait_for_receipt(config, rpc, sale_hash).await?;
    if !sale_receipt.status() {
        return Err(BotError::Transaction(format!(
            "offer fulfillment transaction {sale_hash} reverted"
        )));
    }
    println!("Token {}: SOLD; transaction {sale_hash}", offer.token_id);
    Ok(true)
}

#[allow(clippy::too_many_arguments)]
pub fn calculate_profitability(
    oracle: &PriceOracle,
    prices: &PriceSnapshot,
    config: &MintConfig,
    cost_basis: CostBasis,
    offer: &OpenSeaOffer,
    payout: FulfillmentPayout,
    sell_gas_amount: U256,
    approval_gas_amount: U256,
) -> Result<Profitability> {
    if payout.gross_amount != offer.value
        || payout.fee_amount.checked_add(payout.seller_amount) != Some(payout.gross_amount)
    {
        return Err(BotError::Transaction(
            "OpenSea fulfillment gross payment does not match the selected offer".to_string(),
        ));
    }
    let gross_offer_usd =
        oracle.amount_to_usd(prices, &offer.currency, payout.gross_amount, offer.decimals)?;
    let seller_payout_usd = oracle.amount_to_usd(
        prices,
        &offer.currency,
        payout.seller_amount,
        offer.decimals,
    )?;
    let fee_usd = gross_offer_usd
        .checked_sub(seller_payout_usd)
        .ok_or_else(|| BotError::Transaction("seller payout exceeds gross offer".into()))?;
    let native_currency = config.native_currency_symbol();
    let sell_gas_usd = oracle.cost_to_usd(prices, native_currency, sell_gas_amount, 18)?;
    let approval_gas_usd = oracle.cost_to_usd(prices, native_currency, approval_gas_amount, 18)?;
    let total_cost_usd = cost_basis
        .mint_payment_usd
        .checked_add(cost_basis.mint_gas_usd)
        .and_then(|amount| amount.checked_add(fee_usd))
        .and_then(|amount| amount.checked_add(sell_gas_usd))
        .and_then(|amount| amount.checked_add(approval_gas_usd))
        .ok_or_else(|| BotError::Transaction("USD cost amount overflowed".to_string()))?;
    let post_sale_gas_usd = sell_gas_usd
        .checked_add(approval_gas_usd)
        .ok_or_else(|| BotError::Transaction("USD gas cost overflowed".to_string()))?;
    let net_offer_usd = seller_payout_usd.saturating_sub(post_sale_gas_usd);
    let profit_usd = gross_offer_usd.saturating_sub(total_cost_usd);
    let loss_usd = total_cost_usd.saturating_sub(gross_offer_usd);
    let minimum_profit = parse_usd_amount(&config.auto_sell.min_profit_usd)?;
    let required_gross = total_cost_usd
        .checked_add(minimum_profit)
        .ok_or_else(|| BotError::Transaction("profit threshold overflowed".to_string()))?;
    Ok(Profitability {
        mint_payment_usd: cost_basis.mint_payment_usd,
        mint_gas_usd: cost_basis.mint_gas_usd,
        gross_offer_usd,
        fee_usd,
        sell_gas_usd,
        approval_gas_usd,
        total_cost_usd,
        net_offer_usd,
        profit_usd,
        loss_usd,
        profitable: gross_offer_usd >= required_gross,
    })
}

fn format_profit(profitability: &Profitability) -> String {
    if profitability.loss_usd.is_zero() {
        format!("profit {}", format_usd(profitability.profit_usd))
    } else {
        format!("loss {}", format_usd(profitability.loss_usd))
    }
}

async fn apply_sell_fee_fields(
    config: &MintConfig,
    rpc: &RpcClients,
    request: &mut TransactionRequest,
) -> Result<()> {
    if apply_configured_sell_fee_fields(config, request)? {
        return Ok(());
    }
    let mut estimate = rpc.estimate_eip1559_fees().await?;
    estimate.max_fee_per_gas = scale_u128(estimate.max_fee_per_gas, config.gas.multiplier)?;
    estimate.max_priority_fee_per_gas =
        scale_u128(estimate.max_priority_fee_per_gas, config.gas.multiplier)?;
    request.set_max_fee_per_gas(estimate.max_fee_per_gas);
    request.set_max_priority_fee_per_gas(estimate.max_priority_fee_per_gas);
    Ok(())
}

fn apply_configured_sell_fee_fields(
    config: &MintConfig,
    request: &mut TransactionRequest,
) -> Result<bool> {
    match config.gas.mode {
        GasMode::Legacy => {
            let price = config.gas.gas_price_gwei.as_deref().ok_or_else(|| {
                BotError::Config("gas.gas_price_gwei is required for sell".into())
            })?;
            request.set_gas_price(parse_gwei(price)?);
            Ok(true)
        }
        GasMode::Auto => Ok(false),
        GasMode::Eip1559 | GasMode::Manual => {
            let max_fee =
                config.gas.max_fee_gwei.as_deref().ok_or_else(|| {
                    BotError::Config("gas.max_fee_gwei is required for sell".into())
                })?;
            let priority = config.gas.max_priority_fee_gwei.as_deref().ok_or_else(|| {
                BotError::Config("gas.max_priority_fee_gwei is required for sell".into())
            })?;
            request.set_max_fee_per_gas(parse_gwei(max_fee)?);
            request.set_max_priority_fee_per_gas(parse_gwei(priority)?);
            Ok(true)
        }
    }
}

async fn build_approval_request(
    config: &MintConfig,
    rpc: &RpcClients,
    wallet: Address,
    nft: Address,
    operator: Address,
    nonce: Option<u64>,
) -> Result<TransactionRequest> {
    let function = parse_function("setApprovalForAll(address,bool)")?;
    let input = function
        .abi_encode_input(&[operator.into(), true.into()])
        .map_err(|err| BotError::Abi(format!("setApprovalForAll: {err}")))?;
    let mut request = TransactionRequest::default()
        .with_from(wallet)
        .with_to(nft)
        .with_chain_id(config.chain_id)
        .with_input(input)
        .with_value(U256::ZERO);
    apply_sell_fee_fields(config, rpc, &mut request).await?;
    let gas = scale_u64(
        rpc.estimate_gas(request.clone()).await?,
        config.gas.multiplier,
    )?;
    request.set_gas_limit(gas);
    if let Some(nonce) = nonce {
        request.set_nonce(nonce);
    }
    Ok(request)
}

fn transaction_max_gas_cost(request: &TransactionRequest) -> Result<U256> {
    let gas = request.gas.ok_or_else(|| {
        BotError::Transaction("transaction has no gas limit for profitability".to_string())
    })?;
    let fee = request
        .gas_price
        .or(request.max_fee_per_gas)
        .ok_or_else(|| {
            BotError::Transaction("transaction has no fee cap for profitability".to_string())
        })?;
    U256::from(gas)
        .checked_mul(U256::from(fee))
        .ok_or_else(|| BotError::Transaction("transaction gas cost overflowed".to_string()))
}

async fn transaction_gas_budget(
    config: &MintConfig,
    rpc: &RpcClients,
    request: &TransactionRequest,
) -> Result<U256> {
    let execution = transaction_max_gas_cost(request)?;
    if config.chain_id != crate::config::INK_MAINNET_CHAIN_ID {
        return Ok(execution);
    }
    let input = request.input.input.as_ref().or(request.input.data.as_ref());
    let extra = rpc
        .ink_extra_fee_reserve(
            input.map_or(0, |data| data.len()),
            request.gas.unwrap_or_default(),
        )
        .await?;
    execution
        .checked_add(extra)
        .ok_or_else(|| BotError::Transaction("sell fee budget overflowed".into()))
}

fn receipt_gas_cost(receipt: &TransactionReceipt) -> Result<U256> {
    U256::from(receipt.gas_used)
        .checked_mul(U256::from(receipt.effective_gas_price))
        .ok_or_else(|| BotError::Transaction("confirmed transaction fee overflowed".to_string()))
}

async fn fresh_prices(
    config: &MintConfig,
    oracle: &PriceOracle,
    cost_basis: &RawCostBasis,
    offer_currency: &str,
) -> Result<PriceSnapshot> {
    oracle
        .snapshot(
            &[
                &cost_basis.mint_payment_currency,
                &cost_basis.native_currency,
                offer_currency,
            ],
            &config.auto_sell.currency_usd_prices,
        )
        .await
}

async fn is_approved_for_all(
    rpc: &RpcClients,
    nft: Address,
    owner: Address,
    operator: Address,
) -> Result<bool> {
    let function = parse_function("isApprovedForAll(address,address) returns (bool)")?;
    let input = function
        .abi_encode_input(&[owner.into(), operator.into()])
        .map_err(|err| BotError::Abi(format!("isApprovedForAll: {err}")))?;
    let output = rpc
        .call_at(
            TransactionRequest::default().with_to(nft).with_input(input),
            BlockId::latest(),
        )
        .await?;
    let values = function
        .abi_decode_output(&output)
        .map_err(|err| BotError::Abi(format!("isApprovedForAll output: {err}")))?;
    match values.first() {
        Some(DynSolValue::Bool(value)) => Ok(*value),
        _ => Err(BotError::Abi(
            "isApprovedForAll did not return bool".to_string(),
        )),
    }
}

async fn wallet_owns_token(
    rpc: &RpcClients,
    nft: Address,
    wallet: Address,
    token_id: U256,
) -> Result<bool> {
    let owner_of = parse_function("ownerOf(uint256) returns (address)")?;
    let owner_input = owner_of
        .abi_encode_input(&[token_id.into()])
        .map_err(|err| BotError::Abi(format!("ownerOf: {err}")))?;
    if let Ok(output) = rpc
        .call_at(
            TransactionRequest::default()
                .with_to(nft)
                .with_input(owner_input),
            BlockId::latest(),
        )
        .await
        && let Ok(values) = owner_of.abi_decode_output(&output)
        && let Some(DynSolValue::Address(owner)) = values.first()
    {
        return Ok(*owner == wallet);
    }

    let balance_of = parse_function("balanceOf(address,uint256) returns (uint256)")?;
    let balance_input = balance_of
        .abi_encode_input(&[wallet.into(), token_id.into()])
        .map_err(|err| BotError::Abi(format!("balanceOf: {err}")))?;
    let output = rpc
        .call_at(
            TransactionRequest::default()
                .with_to(nft)
                .with_input(balance_input),
            BlockId::latest(),
        )
        .await?;
    let values = balance_of
        .abi_decode_output(&output)
        .map_err(|err| BotError::Abi(format!("balanceOf output: {err}")))?;
    match values.first() {
        Some(DynSolValue::Uint(balance, _)) => Ok(!balance.is_zero()),
        _ => Err(BotError::Abi(
            "balanceOf did not return an integer".to_string(),
        )),
    }
}

async fn send_request(
    config: &MintConfig,
    rpc: &RpcClients,
    wallet: &LoadedWallet,
    request: TransactionRequest,
) -> Result<B256> {
    let gas_cost = transaction_gas_budget(config, rpc, &request).await?;
    if let Some(cap) = config
        .auto_sell
        .max_sell_gas_cost_native
        .as_deref()
        .or(config.gas.max_total_gas_cost_native.as_deref())
        && gas_cost > parse_native_amount(cap)?
    {
        return Err(BotError::Transaction(
            "auto-sell transaction exceeds the gas cap".into(),
        ));
    }
    let balance = rpc.check_balance(wallet.address).await?;
    let required = gas_cost
        .checked_add(request.value.unwrap_or_default())
        .ok_or_else(|| BotError::Transaction("sell balance budget overflowed".into()))?;
    if balance < required {
        return Err(BotError::InsufficientBalance {
            needed: required.to_string(),
            available: balance.to_string(),
        });
    }
    let signed = wallet.sign_request(request).await?;
    let (hash, _) = rpc.broadcast_raw(signed.encoded_2718()).await?;
    Ok(hash)
}

async fn wait_for_receipt(
    config: &MintConfig,
    rpc: &RpcClients,
    hash: B256,
) -> Result<TransactionReceipt> {
    let monitor = async {
        loop {
            tokio::time::sleep(Duration::from_millis(500)).await;
            let receipt = match rpc.transaction_receipt(hash).await {
                Ok(Some(receipt))
                    if receipt.block_number.is_some() && receipt.block_hash.is_some() =>
                {
                    receipt
                }
                Ok(_) => continue,
                Err(error) => {
                    tracing::warn!(%hash, %error, "receipt lookup failed; retrying within the deadline");
                    continue;
                }
            };
            if config.confirmations <= 1 {
                return receipt;
            }
            let target = receipt
                .block_number
                .unwrap_or_default()
                .saturating_add(config.confirmations.saturating_sub(1));
            let Ok(current) = rpc.block_number().await else {
                continue;
            };
            if current < target {
                continue;
            }
            if let Ok(Some(canonical)) = rpc.transaction_receipt(hash).await
                && canonical.block_number == receipt.block_number
                && canonical.block_hash == receipt.block_hash
            {
                return canonical;
            }
        }
    };
    tokio::select! {
        receipt = tokio::time::timeout(Duration::from_secs(config.auto_sell.receipt_timeout_seconds), monitor) =>
            receipt.map_err(|_| BotError::BroadcastOutcomeUnknown { hash }),
        _ = tokio::signal::ctrl_c() => Err(BotError::BroadcastOutcomeUnknown { hash }),
    }
}

/// Derive the exact seller payout encoded by OpenSea's fulfillment payload.
/// For bids, Seaport transfers the offered ERC-20 to the fulfiller after
/// routing same-token consideration/additional-recipient amounts to fee
/// recipients. Collection metadata is deliberately not used here: the signed
/// order is the source of truth for both required and optional creator fees.
pub fn fulfillment_payout(
    fulfillment: &OpenSeaOfferFulfillment,
    offer: &OpenSeaOffer,
    seller: Address,
) -> Result<FulfillmentPayout> {
    let function = fulfillment.transaction.function.to_ascii_lowercase();
    if function.starts_with("fulfillbasicorder") {
        basic_fulfillment_payout(&fulfillment.transaction.input_data, offer, seller)
    } else if function.starts_with("fulfilladvancedorder") {
        standard_fulfillment_payout(&fulfillment.transaction.input_data, offer, seller, true)
    } else if function.starts_with("fulfillorder") {
        standard_fulfillment_payout(&fulfillment.transaction.input_data, offer, seller, false)
    } else if function.starts_with("matchadvancedorders") {
        match_advanced_fulfillment_payout(
            &fulfillment.transaction.input_data,
            fulfillment.orders.as_slice(),
            offer,
            seller,
        )
    } else {
        Err(BotError::Transaction(format!(
            "cannot derive payout for unsupported fulfillment function `{}`",
            fulfillment.transaction.function
        )))
    }
}

/// Derive the seller payout for Seaport's batched `matchAdvancedOrders`
/// fulfillment. OpenSea uses this path for some collection/criteria offers:
/// the buyer's payment offer is matched against a seller-side counter-order,
/// so the payment consideration items can live in a different order than the
/// selected offer. We only accept a fixed-price payment item that exactly
/// matches the API offer and only count consideration items connected to that
/// payment through the returned fulfillment components.
fn match_advanced_fulfillment_payout(
    input: &Value,
    fallback_orders: &[Value],
    offer: &OpenSeaOffer,
    seller: Address,
) -> Result<FulfillmentPayout> {
    let orders = match_order_values(input, fallback_orders)?;
    let resolvers = object_field(input, &["criteriaResolvers", "criteria_resolvers"])
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let views = orders
        .iter()
        .map(match_advanced_order_view)
        .collect::<Result<Vec<_>>>()?;
    validate_match_advanced_nft(&views, resolvers, offer)?;

    let mut payment_candidates = Vec::new();
    for (order_index, view) in views.iter().enumerate() {
        let offered = required(view.parameters, "offer")?
            .as_array()
            .ok_or_else(|| BotError::Transaction("Seaport offer is not an array".to_string()))?;
        for (item_index, item) in offered.iter().enumerate() {
            let item = item.as_object().ok_or_else(|| {
                BotError::Transaction("Seaport offer item is invalid".to_string())
            })?;
            let item_type = parse_json_amount(required_any(item, &["itemType"])?)?;
            if item_type > U256::from(1u64) {
                continue;
            }
            let amount =
                mul_div_floor(fixed_order_amount(item)?, view.numerator, view.denominator)?;
            if amount == offer.value {
                payment_candidates.push((
                    order_index,
                    item_index,
                    item_type,
                    parse_json_address(required_any(item, &["token"])?)?,
                    parse_json_amount(required_any(item, &["identifierOrCriteria"])?)?,
                ));
            }
        }
    }
    if payment_candidates.len() != 1 {
        return Err(BotError::Transaction(format!(
            "matchAdvancedOrders must contain exactly one offered payment item matching the OpenSea offer (found {})",
            payment_candidates.len()
        )));
    }
    let (payment_order, payment_item, payment_type, payment_token, payment_identifier) =
        payment_candidates.remove(0);

    let fulfillments = object_field(input, &["fulfillments"])
        .and_then(Value::as_array)
        .ok_or_else(|| {
            BotError::Transaction("matchAdvancedOrders has no fulfillments".to_string())
        })?;
    let mut seller_amount = U256::ZERO;
    let mut fee_amount = U256::ZERO;
    let mut matched_payment = false;
    for fulfillment in fulfillments {
        let object = fulfillment.as_object().ok_or_else(|| {
            BotError::Transaction("Seaport fulfillment component is invalid".to_string())
        })?;
        let offer_components = required_any(object, &["offerComponents", "offer_components"])?
            .as_array()
            .ok_or_else(|| BotError::Transaction("offerComponents is not an array".to_string()))?;
        if !offer_components.iter().any(|component| {
            fulfillment_component_indices(component)
                .map(|(order_index, item_index)| {
                    order_index == U256::from(payment_order)
                        && item_index == U256::from(payment_item)
                })
                .unwrap_or(false)
        }) {
            continue;
        }
        matched_payment = true;
        let consideration_components = required_any(
            object,
            &["considerationComponents", "consideration_components"],
        )?
        .as_array()
        .ok_or_else(|| {
            BotError::Transaction("considerationComponents is not an array".to_string())
        })?;
        for component in consideration_components {
            let (order_index, item_index) = fulfillment_component_indices(component)?;
            let view = views.get(u256_to_usize(order_index)?).ok_or_else(|| {
                BotError::Transaction("matchAdvancedOrders references an invalid order".to_string())
            })?;
            let consideration = required(view.parameters, "consideration")?
                .as_array()
                .ok_or_else(|| {
                    BotError::Transaction("Seaport consideration is not an array".to_string())
                })?;
            let item = consideration
                .get(u256_to_usize(item_index)?)
                .ok_or_else(|| {
                    BotError::Transaction(
                        "matchAdvancedOrders references an invalid consideration item".to_string(),
                    )
                })?
                .as_object()
                .ok_or_else(|| {
                    BotError::Transaction("Seaport consideration item is invalid".to_string())
                })?;
            if !payment_item_matches(item, payment_type, payment_token, payment_identifier)? {
                continue;
            }
            let amount = mul_div_ceil(fixed_order_amount(item)?, view.numerator, view.denominator)?;
            let recipient = parse_json_address(required_any(item, &["recipient"])?)?;
            if recipient == seller {
                seller_amount = seller_amount.checked_add(amount).ok_or_else(|| {
                    BotError::Transaction("fulfillment seller amount overflowed".to_string())
                })?;
            } else {
                fee_amount = fee_amount.checked_add(amount).ok_or_else(|| {
                    BotError::Transaction("fulfillment fee amount overflowed".to_string())
                })?;
            }
        }
    }
    if !matched_payment {
        return Err(BotError::Transaction(
            "matchAdvancedOrders does not connect the offered payment to consideration".to_string(),
        ));
    }
    let gross_amount = offer.value;
    let expected_seller_amount = gross_amount.checked_sub(fee_amount).ok_or_else(|| {
        BotError::Transaction("fulfillment fees exceed gross offer amount".to_string())
    })?;
    if seller_amount != expected_seller_amount {
        return Err(BotError::Transaction(format!(
            "matchAdvancedOrders seller consideration is {seller_amount}, expected {expected_seller_amount}"
        )));
    }
    payout_from_amounts(gross_amount, fee_amount)
}

struct MatchAdvancedOrderView<'a> {
    parameters: &'a Map<String, Value>,
    numerator: U256,
    denominator: U256,
}

fn match_order_values<'a>(input: &'a Value, fallback: &'a [Value]) -> Result<&'a [Value]> {
    if let Some(orders) = object_field(input, &["orders", "advancedOrders", "advanced_orders"])
        .and_then(Value::as_array)
    {
        return Ok(orders.as_slice());
    }
    if fallback.is_empty() {
        return Err(BotError::Transaction(
            "matchAdvancedOrders has no orders".to_string(),
        ));
    }
    Ok(fallback)
}

fn match_advanced_order_view(value: &Value) -> Result<MatchAdvancedOrderView<'_>> {
    value
        .as_object()
        .ok_or_else(|| BotError::Transaction("Seaport advanced order is invalid".to_string()))?;
    let nested = object_field(value, &["order"]).unwrap_or(value);
    let parameters = object_field(nested, &["parameters"])
        .unwrap_or(nested)
        .as_object()
        .ok_or_else(|| BotError::Transaction("Seaport parameters are invalid".to_string()))?;
    let numerator = parse_json_amount(
        object_field(value, &["numerator"])
            .or_else(|| object_field(nested, &["numerator"]))
            .ok_or_else(|| BotError::Transaction("advanced order has no numerator".to_string()))?,
    )?;
    let denominator = parse_json_amount(
        object_field(value, &["denominator"])
            .or_else(|| object_field(nested, &["denominator"]))
            .ok_or_else(|| {
                BotError::Transaction("advanced order has no denominator".to_string())
            })?,
    )?;
    if numerator.is_zero() || denominator.is_zero() || numerator > denominator {
        return Err(BotError::Transaction(
            "advanced fulfillment has an invalid fill fraction".to_string(),
        ));
    }
    Ok(MatchAdvancedOrderView {
        parameters,
        numerator,
        denominator,
    })
}

fn payment_item_matches(
    item: &Map<String, Value>,
    item_type: U256,
    token: Address,
    identifier: U256,
) -> Result<bool> {
    Ok(
        parse_json_amount(required_any(item, &["itemType"])?)? == item_type
            && parse_json_address(required_any(item, &["token"])?)? == token
            && parse_json_amount(required_any(item, &["identifierOrCriteria"])?)? == identifier,
    )
}

fn fulfillment_component_indices(value: &Value) -> Result<(U256, U256)> {
    let object = value.as_object().ok_or_else(|| {
        BotError::Transaction("Seaport fulfillment component is invalid".to_string())
    })?;
    Ok((
        parse_json_amount(required_any(object, &["orderIndex", "order_index"])?)?,
        parse_json_amount(required_any(object, &["itemIndex", "item_index"])?)?,
    ))
}

fn u256_to_usize(value: U256) -> Result<usize> {
    value
        .try_into()
        .map_err(|_| BotError::Transaction("matchAdvancedOrders index is too large".to_string()))
}

fn validate_match_advanced_nft(
    orders: &[MatchAdvancedOrderView<'_>],
    resolvers: &[Value],
    offer: &OpenSeaOffer,
) -> Result<()> {
    for (order_index, view) in orders.iter().enumerate() {
        for (side, field) in [(0u64, "offer"), (1u64, "consideration")] {
            let items = required(view.parameters, field)?
                .as_array()
                .ok_or_else(|| BotError::Transaction(format!("Seaport {field} is not an array")))?;
            for (item_index, item) in items.iter().enumerate() {
                let item = item
                    .as_object()
                    .ok_or_else(|| BotError::Transaction("Seaport item is invalid".to_string()))?;
                if parse_json_address(required_any(item, &["token"])?)? != offer.contract_address {
                    continue;
                }
                let item_type = parse_json_amount(required_any(item, &["itemType"])?)?;
                let identifier = parse_json_amount(required_any(item, &["identifierOrCriteria"])?)?;
                let direct = (item_type == U256::from(2u64) || item_type == U256::from(3u64))
                    && identifier == offer.token_id;
                let criteria = (item_type == U256::from(4u64) || item_type == U256::from(5u64))
                    && resolvers.iter().any(|resolver| {
                        let Some(resolver) = resolver.as_object() else {
                            return false;
                        };
                        let parsed = |keys: &[&str]| {
                            keys.iter()
                                .find_map(|key| resolver.get(*key))
                                .and_then(|value| parse_json_amount(value).ok())
                        };
                        parsed(&["orderIndex", "order_index"]) == Some(U256::from(order_index))
                            && parsed(&["side"]) == Some(U256::from(side))
                            && parsed(&["index", "itemIndex", "item_index"])
                                == Some(U256::from(item_index))
                            && parsed(&["identifier"]) == Some(offer.token_id)
                    });
                if direct || criteria {
                    return Ok(());
                }
            }
        }
    }
    Err(BotError::Transaction(
        "OpenSea matchAdvancedOrders does not resolve to the selected NFT".to_string(),
    ))
}

fn basic_fulfillment_payout(
    input: &Value,
    offer: &OpenSeaOffer,
    seller: Address,
) -> Result<FulfillmentPayout> {
    let parameters = object_field(input, &["parameters", "basicOrderParameters"])
        .and_then(Value::as_object)
        .ok_or_else(|| BotError::Transaction("basic fulfillment has no parameters".to_string()))?;
    let basic_order_type = parse_json_amount(required(parameters, "basicOrderType")?)?;
    let route = basic_order_type
        .checked_div(U256::from(4u64))
        .ok_or_else(|| BotError::Transaction("invalid basic order route".to_string()))?;
    if route != U256::from(4u64) && route != U256::from(5u64) {
        return Err(BotError::Transaction(
            "OpenSea fulfillment is not an ERC-20-for-NFT offer".to_string(),
        ));
    }
    let nft_contract = parse_json_address(required(parameters, "considerationToken")?)?;
    let token_id = parse_json_amount(required(parameters, "considerationIdentifier")?)?;
    if nft_contract != offer.contract_address || token_id != offer.token_id {
        return Err(BotError::Transaction(
            "OpenSea basic fulfillment transfers a different NFT".to_string(),
        ));
    }
    let gross_amount = parse_json_amount(required(parameters, "offerAmount")?)?;
    let recipients = required(parameters, "additionalRecipients")?
        .as_array()
        .ok_or_else(|| {
            BotError::Transaction("basic fulfillment recipients are not an array".to_string())
        })?;
    let fee_amount = recipients.iter().try_fold(U256::ZERO, |total, recipient| {
        let recipient = recipient.as_object().ok_or_else(|| {
            BotError::Transaction("basic fulfillment recipient is invalid".to_string())
        })?;
        let address = parse_json_address(required(recipient, "recipient")?)?;
        let amount = parse_json_amount(required(recipient, "amount")?)?;
        if address == seller {
            Ok(total)
        } else {
            total.checked_add(amount).ok_or_else(|| {
                BotError::Transaction("fulfillment fee amount overflowed".to_string())
            })
        }
    })?;
    payout_from_amounts(gross_amount, fee_amount)
}

fn standard_fulfillment_payout(
    input: &Value,
    offer: &OpenSeaOffer,
    seller: Address,
    advanced: bool,
) -> Result<FulfillmentPayout> {
    let order = if advanced {
        object_field(input, &["advancedOrder", "advanced_order"])
    } else {
        object_field(input, &["order"])
    }
    .ok_or_else(|| BotError::Transaction("fulfillment has no Seaport order".to_string()))?;
    let order_object = order
        .as_object()
        .ok_or_else(|| BotError::Transaction("Seaport order is invalid".to_string()))?;
    let parameters = object_field(order, &["parameters"])
        .unwrap_or(order)
        .as_object()
        .ok_or_else(|| BotError::Transaction("Seaport parameters are invalid".to_string()))?;
    let (numerator, denominator) = if advanced {
        let numerator = parse_json_amount(required(order_object, "numerator")?)?;
        let denominator = parse_json_amount(required(order_object, "denominator")?)?;
        if numerator.is_zero() || denominator.is_zero() || numerator > denominator {
            return Err(BotError::Transaction(
                "advanced fulfillment has an invalid fill fraction".to_string(),
            ));
        }
        (numerator, denominator)
    } else {
        (U256::from(1u64), U256::from(1u64))
    };
    let offered_items = required(parameters, "offer")?
        .as_array()
        .ok_or_else(|| BotError::Transaction("Seaport offer is not an array".to_string()))?;
    let payment_items = offered_items
        .iter()
        .filter(|item| {
            item.get("itemType")
                .and_then(|value| parse_json_amount(value).ok())
                .is_some_and(|item_type| item_type <= U256::from(1u64))
        })
        .collect::<Vec<_>>();
    if payment_items.len() != 1 {
        return Err(BotError::Transaction(
            "OpenSea fulfillment must contain exactly one offered payment item".to_string(),
        ));
    }
    let payment = payment_items[0]
        .as_object()
        .ok_or_else(|| BotError::Transaction("Seaport payment item is invalid".to_string()))?;
    let payment_type = parse_json_amount(required(payment, "itemType")?)?;
    let payment_token = parse_json_address(required(payment, "token")?)?;
    let payment_identifier = parse_json_amount(required(payment, "identifierOrCriteria")?)?;
    let gross_base = fixed_order_amount(payment)?;
    let gross_amount = mul_div_floor(gross_base, numerator, denominator)?;

    let consideration = required(parameters, "consideration")?
        .as_array()
        .ok_or_else(|| {
            BotError::Transaction("Seaport consideration is not an array".to_string())
        })?;
    validate_standard_nft(consideration, input, offer)?;
    let fee_amount = consideration.iter().try_fold(U256::ZERO, |total, item| {
        let item = item.as_object().ok_or_else(|| {
            BotError::Transaction("Seaport consideration item is invalid".to_string())
        })?;
        let item_type = parse_json_amount(required(item, "itemType")?)?;
        let token = parse_json_address(required(item, "token")?)?;
        let identifier = parse_json_amount(required(item, "identifierOrCriteria")?)?;
        let recipient = parse_json_address(required(item, "recipient")?)?;
        if item_type == payment_type
            && token == payment_token
            && identifier == payment_identifier
            && recipient != seller
        {
            let amount = mul_div_ceil(fixed_order_amount(item)?, numerator, denominator)?;
            total.checked_add(amount).ok_or_else(|| {
                BotError::Transaction("fulfillment fee amount overflowed".to_string())
            })
        } else {
            Ok(total)
        }
    })?;
    payout_from_amounts(gross_amount, fee_amount)
}

fn validate_standard_nft(
    consideration: &[Value],
    input: &Value,
    offer: &OpenSeaOffer,
) -> Result<()> {
    let resolvers = object_field(input, &["criteriaResolvers", "criteria_resolvers"])
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let valid = consideration.iter().enumerate().any(|(item_index, item)| {
        let Some(item) = item.as_object() else {
            return false;
        };
        let item_type = required(item, "itemType")
            .ok()
            .and_then(|value| parse_json_amount(value).ok());
        let token = required(item, "token")
            .ok()
            .and_then(|value| parse_json_address(value).ok());
        let identifier = required(item, "identifierOrCriteria")
            .ok()
            .and_then(|value| parse_json_amount(value).ok());
        token == Some(offer.contract_address)
            && match item_type {
                Some(value) if value == U256::from(2u64) || value == U256::from(3u64) => {
                    identifier == Some(offer.token_id)
                }
                Some(value) if value == U256::from(4u64) || value == U256::from(5u64) => {
                    resolvers.iter().any(|resolver| {
                        let parsed = |field: &str| {
                            resolver
                                .get(field)
                                .and_then(|value| parse_json_amount(value).ok())
                        };
                        parsed("orderIndex") == Some(U256::ZERO)
                            && parsed("side") == Some(U256::from(1u64))
                            && parsed("index") == Some(U256::from(item_index))
                            && parsed("identifier") == Some(offer.token_id)
                    })
                }
                _ => false,
            }
    });
    if !valid {
        return Err(BotError::Transaction(
            "OpenSea fulfillment does not resolve to the selected NFT".to_string(),
        ));
    }
    Ok(())
}

fn fixed_order_amount(item: &Map<String, Value>) -> Result<U256> {
    let start = parse_json_amount(required(item, "startAmount")?)?;
    let end = parse_json_amount(required(item, "endAmount")?)?;
    if start != end {
        return Err(BotError::Transaction(
            "time-varying Seaport payment amounts are not supported safely".to_string(),
        ));
    }
    Ok(start)
}

fn mul_div_floor(amount: U256, numerator: U256, denominator: U256) -> Result<U256> {
    amount
        .checked_mul(numerator)
        .and_then(|value| value.checked_div(denominator))
        .ok_or_else(|| BotError::Transaction("partial-fill amount overflowed".to_string()))
}

fn mul_div_ceil(amount: U256, numerator: U256, denominator: U256) -> Result<U256> {
    let product = amount
        .checked_mul(numerator)
        .ok_or_else(|| BotError::Transaction("partial-fill amount overflowed".to_string()))?;
    let adjustment = denominator
        .checked_sub(U256::from(1u64))
        .ok_or_else(|| BotError::Transaction("invalid partial-fill denominator".to_string()))?;
    product
        .checked_add(adjustment)
        .and_then(|value| value.checked_div(denominator))
        .ok_or_else(|| BotError::Transaction("partial-fill amount overflowed".to_string()))
}

fn payout_from_amounts(gross_amount: U256, fee_amount: U256) -> Result<FulfillmentPayout> {
    let seller_amount = gross_amount.checked_sub(fee_amount).ok_or_else(|| {
        BotError::Transaction("fulfillment fees exceed gross offer amount".to_string())
    })?;
    Ok(FulfillmentPayout {
        gross_amount,
        fee_amount,
        seller_amount,
    })
}

fn parse_json_amount(value: &Value) -> Result<U256> {
    json_text(value)?
        .parse::<U256>()
        .map_err(|_| BotError::Transaction("fulfillment amount is invalid".to_string()))
}

fn parse_json_address(value: &Value) -> Result<Address> {
    value
        .as_str()
        .ok_or_else(|| BotError::Transaction("fulfillment address is invalid".to_string()))?
        .parse::<Address>()
        .map_err(|_| BotError::Transaction("fulfillment address is invalid".to_string()))
}

pub fn minted_token_ids(
    receipt: &TransactionReceipt,
    contract: Address,
    recipient: Address,
) -> Vec<U256> {
    let transfer = event_topic("Transfer(address,address,uint256)");
    let transfer_single = event_topic("TransferSingle(address,address,address,uint256,uint256)");
    let transfer_batch = event_topic("TransferBatch(address,address,address,uint256[],uint256[])");
    let mut ids = Vec::new();
    for log in receipt.inner.logs() {
        let log = log.clone();
        if log.address() != contract || log.topics().is_empty() {
            continue;
        }
        if log.topics()[0] == transfer && log.topics().len() >= 4 {
            if topic_address(log.topics()[2]) == Some(recipient) {
                ids.push(U256::from_be_bytes(log.topics()[3].0));
            }
        } else if log.topics()[0] == transfer_single && log.topics().len() == 4 {
            if topic_address(log.topics()[3]) == Some(recipient)
                && log.data().data.len() == 64
                && !U256::from_be_slice(&log.data().data[32..]).is_zero()
            {
                ids.push(U256::from_be_slice(&log.data().data[..32]));
            }
        } else if log.topics()[0] == transfer_batch
            && log.topics().len() == 4
            && topic_address(log.topics()[3]) == Some(recipient)
        {
            ids.extend(decode_batch_ids(&log.data().data));
        }
    }
    ids.sort_unstable();
    ids.dedup();
    ids
}

fn decode_batch_ids(data: &[u8]) -> Vec<U256> {
    fn array(data: &[u8], word: usize) -> Option<&[u8]> {
        let offset: usize = U256::from_be_slice(data.get(word..word + 32)?)
            .try_into()
            .ok()?;
        if offset < 64 || !offset.is_multiple_of(32) {
            return None;
        }
        let start = offset.checked_add(32)?;
        let count: usize = U256::from_be_slice(data.get(offset..start)?)
            .try_into()
            .ok()?;
        let end = start.checked_add(count.checked_mul(32)?)?;
        data.get(start..end)
    }
    let (Some(ids), Some(amounts)) = (array(data, 0), array(data, 32)) else {
        return Vec::new();
    };
    if ids.len() != amounts.len() {
        return Vec::new();
    }
    ids.as_chunks::<32>()
        .0
        .iter()
        .zip(amounts.as_chunks::<32>().0.iter())
        .filter(|(_, amount)| !U256::from_be_slice(amount.as_slice()).is_zero())
        .map(|(id, _)| U256::from_be_slice(id))
        .collect()
}

fn topic_address(topic: B256) -> Option<Address> {
    Some(Address::from_slice(topic.0.get(12..32)?))
}

fn event_topic(signature: &str) -> B256 {
    keccak256(signature.as_bytes())
}

fn expected_chain_slug(config: &MintConfig) -> Option<&str> {
    config
        .auto_sell
        .opensea_chain
        .as_deref()
        .or_else(|| known_chain_slug(config.chain_id))
}

// Canonical Seaport 1.6 deployment and ABI, from ProjectOpenSea/seaport.
const SEAPORT: Address = alloy::primitives::address!("0000000000000068F116a894984e2DB1123eB395");
const ORDER_PARAMETERS: &str = "(address,address,(uint8,address,uint256,uint256,uint256)[],(uint8,address,uint256,uint256,uint256,address)[],uint8,uint256,uint256,bytes32,uint256,bytes32,uint256)";
const BASIC_PARAMETERS: &str = "(address,uint256,uint256,address,address,address,uint256,uint256,uint8,uint256,uint256,bytes32,uint256,bytes32,bytes32,uint256,(uint256,address)[],bytes)";
const RESOLVERS: &str = "(uint256,uint8,uint256,uint256,bytes32[])[]";

fn known_payment_token(chain_id: u64, currency: &str) -> Option<Address> {
    // OpenSea's documented Ethereum WETH contract; other chains/assets must
    // be explicitly bound by the operator, even when symbols are identical.
    (chain_id == 1 && currency.eq_ignore_ascii_case("WETH")).then_some(alloy::primitives::address!(
        "C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"
    ))
}

async fn validate_offer_decimals(
    config: &MintConfig,
    rpc: &RpcClients,
    offer: &OpenSeaOffer,
) -> Result<()> {
    let mut protocol = config.clone();
    protocol.contract_address = SEAPORT.to_string();
    protocol.expected_contract_code_hash = None;
    rpc.validate_contract(&protocol).await?;
    let token = config
        .auto_sell
        .currency_token_addresses
        .iter()
        .find(|(symbol, _)| symbol.eq_ignore_ascii_case(&offer.currency))
        .and_then(|(_, address)| address.parse::<Address>().ok())
        .or_else(|| known_payment_token(config.chain_id, &offer.currency))
        .ok_or_else(|| BotError::Config("offer currency has no trusted token address".into()))?;
    let function = parse_function("decimals() returns (uint8)")?;
    let output = rpc
        .call_at(
            TransactionRequest::default()
                .with_to(token)
                .with_input(function.selector().to_vec()),
            BlockId::latest(),
        )
        .await?;
    let decimals = function
        .abi_decode_output(&output)
        .map_err(|_| BotError::Transaction("ERC-20 decimals query failed".into()))?;
    if decimals.first() != Some(&DynSolValue::Uint(U256::from(offer.decimals), 8)) {
        return Err(BotError::Transaction(
            "offer decimals do not match the trusted ERC-20 contract".into(),
        ));
    }
    Ok(())
}

fn validate_fulfillment_function(function: &alloy::json_abi::Function) -> Result<()> {
    let advanced = format!("({ORDER_PARAMETERS},uint120,uint120,bytes,bytes)");
    let expected = match function.name.as_str() {
        "fulfillBasicOrder" | "fulfillBasicOrder_efficient_6GL6yc" => {
            format!("{}({BASIC_PARAMETERS})", function.name)
        }
        "fulfillOrder" => format!("fulfillOrder(({ORDER_PARAMETERS},bytes),bytes32)"),
        "fulfillAdvancedOrder" => {
            format!("fulfillAdvancedOrder({advanced},{RESOLVERS},bytes32,address)")
        }
        "matchAdvancedOrders" => format!(
            "matchAdvancedOrders({advanced}[],{RESOLVERS},((uint256,uint256)[],(uint256,uint256)[])[],address)"
        ),
        _ => return Err(BotError::Transaction("unsupported Seaport function".into())),
    };
    if function.signature() != expected {
        return Err(BotError::Transaction(
            "fulfillment does not match the canonical Seaport ABI".into(),
        ));
    }
    Ok(())
}

fn validate_fulfillment_policy(
    config: &MintConfig,
    fulfillment: &OpenSeaOfferFulfillment,
    offer: &OpenSeaOffer,
    seller: Address,
) -> Result<()> {
    validate_fulfillment_chain(fulfillment, config.chain_id)?;
    let tx = &fulfillment.transaction;
    if tx.to != SEAPORT
        || offer.protocol_address != SEAPORT
        || !fulfillment.protocol.eq_ignore_ascii_case("seaport1.6")
        || !tx.value.is_zero()
    {
        return Err(BotError::Transaction(
            "auto-sell requires a zero-value call to canonical Seaport 1.6".into(),
        ));
    }
    let function = parse_function(&tx.function)?;
    validate_fulfillment_function(&function)?;
    let payment_token = config.auto_sell.currency_token_addresses.iter()
        .find(|(symbol, _)| symbol.eq_ignore_ascii_case(&offer.currency))
        .and_then(|(_, address)| address.parse::<Address>().ok())
        .filter(|address| !address.is_zero())
        .or_else(|| known_payment_token(config.chain_id, &offer.currency))
        .ok_or_else(|| BotError::Config(format!(
            "configure auto_sell.currency_token_addresses.{} with the trusted ERC-20 address on this chain before selling", offer.currency
        )))?;
    let input = &tx.input_data;
    if function.name.starts_with("fulfillBasicOrder") {
        let p = object_field(input, &["parameters", "basicOrderParameters"])
            .and_then(Value::as_object)
            .ok_or_else(|| BotError::Transaction("missing basic parameters".into()))?;
        if parse_json_address(required(p, "offerToken")?)? != payment_token
            || !parse_json_amount(required(p, "offerIdentifier")?)?.is_zero()
            || parse_json_amount(required(p, "considerationAmount")?)? != U256::from(1)
            || parse_json_address(required(p, "offerer")?)? == seller
        {
            return Err(BotError::Transaction(
                "basic offer has an unexpected payment, seller, or NFT quantity".into(),
            ));
        }
    } else if function.name == "matchAdvancedOrders" {
        validate_match_assets(input, &fulfillment.orders, offer, seller, payment_token)?;
    } else {
        let order = object_field(input, &["advancedOrder", "advanced_order", "order"])
            .ok_or_else(|| BotError::Transaction("missing order".into()))?;
        let p = object_field(order, &["parameters"])
            .unwrap_or(order)
            .as_object()
            .ok_or_else(|| BotError::Transaction("invalid order".into()))?;
        if parse_json_address(required(p, "offerer")?)? == seller {
            return Err(BotError::Transaction(
                "cannot accept the seller's own offer".into(),
            ));
        }
        let (n, d) = if function.name == "fulfillAdvancedOrder" {
            let view = match_advanced_order_view(order)?;
            (view.numerator, view.denominator)
        } else {
            (U256::from(1), U256::from(1))
        };
        let offered = required(p, "offer")?
            .as_array()
            .ok_or_else(|| BotError::Transaction("invalid offer items".into()))?;
        if offered.len() != 1 || !trusted_payment(&offered[0], payment_token)? {
            return Err(BotError::Transaction(
                "offer must pay only the configured ERC-20 token".into(),
            ));
        }
        let consideration = required(p, "consideration")?
            .as_array()
            .ok_or_else(|| BotError::Transaction("invalid consideration".into()))?;
        let mut nft_count = 0;
        for (index, item) in consideration.iter().enumerate() {
            if trusted_payment(item, payment_token)? {
                continue;
            }
            validate_selected_nft(item, input, offer, 0, 1, index, n, d)?;
            nft_count += 1;
        }
        if nft_count != 1 {
            return Err(BotError::Transaction(
                "offer must transfer exactly one selected NFT".into(),
            ));
        }
    }
    // Payout checks are part of the policy, before any approval is granted.
    let payout = fulfillment_payout(fulfillment, offer, seller)?;
    if payout.gross_amount != offer.value
        || payout.fee_amount.checked_add(payout.seller_amount) != Some(payout.gross_amount)
    {
        return Err(BotError::Transaction(
            "fulfillment payment differs from the selected offer".into(),
        ));
    }
    Ok(())
}

fn trusted_payment(item: &Value, token: Address) -> Result<bool> {
    let item = item
        .as_object()
        .ok_or_else(|| BotError::Transaction("invalid Seaport item".into()))?;
    payment_item_matches(item, U256::from(1), token, U256::ZERO)
}

#[allow(clippy::too_many_arguments)]
fn validate_selected_nft(
    item: &Value,
    input: &Value,
    offer: &OpenSeaOffer,
    order: usize,
    side: u64,
    index: usize,
    n: U256,
    d: U256,
) -> Result<()> {
    let item = item
        .as_object()
        .ok_or_else(|| BotError::Transaction("invalid NFT item".into()))?;
    let ty = parse_json_amount(required(item, "itemType")?)?;
    let id = parse_json_amount(required(item, "identifierOrCriteria")?)?;
    let resolved = if ty == U256::from(4) || ty == U256::from(5) {
        object_field(input, &["criteriaResolvers", "criteria_resolvers"])
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .any(|r| {
                let num =
                    |keys: &[&str]| object_field(r, keys).and_then(|v| parse_json_amount(v).ok());
                num(&["orderIndex", "order_index"]) == Some(U256::from(order))
                    && num(&["side"]) == Some(U256::from(side))
                    && num(&["index", "itemIndex", "item_index"]) == Some(U256::from(index))
                    && num(&["identifier"]) == Some(offer.token_id)
            })
    } else {
        (ty == U256::from(2) || ty == U256::from(3)) && id == offer.token_id
    };
    let amount = fixed_order_amount(item)?
        .checked_mul(n)
        .ok_or_else(|| BotError::Transaction("NFT quantity overflowed".into()))?;
    if !resolved
        || parse_json_address(required(item, "token")?)? != offer.contract_address
        || d.is_zero()
        || amount != d
    {
        return Err(BotError::Transaction(
            "fulfillment transfers an unexpected NFT or quantity".into(),
        ));
    }
    Ok(())
}

fn validate_match_assets(
    input: &Value,
    fallback: &[Value],
    offer: &OpenSeaOffer,
    seller: Address,
    payment: Address,
) -> Result<()> {
    let orders = match_order_values(input, fallback)?;
    if orders.len() != 2 {
        return Err(BotError::Transaction(
            "auto-sell matching requires exactly one buyer and one seller order".into(),
        ));
    }
    let mut seller_order = None;
    let mut sizes = Vec::new();
    for (i, order) in orders.iter().enumerate() {
        let view = match_advanced_order_view(order)?;
        let is_seller = parse_json_address(required(view.parameters, "offerer")?)? == seller;
        if is_seller && seller_order.replace(i).is_some() {
            return Err(BotError::Transaction(
                "multiple seller orders are not supported".into(),
            ));
        }
        let mut lengths = [0; 2];
        for (side, field) in ["offer", "consideration"].iter().enumerate() {
            let items = required(view.parameters, field)?
                .as_array()
                .ok_or_else(|| BotError::Transaction("invalid order items".into()))?;
            lengths[side] = items.len();
            let nft_side = (is_seller && side == 0) || (!is_seller && side == 1);
            if items.is_empty() || (nft_side && items.len() != 1) || (side == 0 && items.len() != 1)
            {
                return Err(BotError::Transaction(
                    "unexpected match order assets".into(),
                ));
            }
            for (j, item) in items.iter().enumerate() {
                if nft_side {
                    validate_selected_nft(
                        item,
                        input,
                        offer,
                        i,
                        side as u64,
                        j,
                        view.numerator,
                        view.denominator,
                    )?;
                } else if !trusted_payment(item, payment)? {
                    return Err(BotError::Transaction(
                        "match order contains an untrusted payment token".into(),
                    ));
                }
            }
        }
        sizes.push(lengths);
    }
    let seller_order =
        seller_order.ok_or_else(|| BotError::Transaction("match has no seller order".into()))?;
    // Every item must participate exactly once. Cross-order connections must
    // route the buyer's currency to seller consideration and the NFT back.
    let mut used = std::collections::BTreeSet::new();
    let fulfillments = required(
        input
            .as_object()
            .ok_or_else(|| BotError::Transaction("invalid input".into()))?,
        "fulfillments",
    )?
    .as_array()
    .ok_or_else(|| BotError::Transaction("invalid fulfillments".into()))?;
    for f in fulfillments {
        let f = f
            .as_object()
            .ok_or_else(|| BotError::Transaction("invalid fulfillment".into()))?;
        let offered = required_any(f, &["offerComponents", "offer_components"])?
            .as_array()
            .ok_or_else(|| BotError::Transaction("invalid components".into()))?;
        if offered.len() != 1 {
            return Err(BotError::Transaction("ambiguous payment components".into()));
        }
        let source = u256_to_usize(fulfillment_component_indices(&offered[0])?.0)?;
        for (side, keys) in [
            (0, &["offerComponents", "offer_components"][..]),
            (
                1,
                &["considerationComponents", "consideration_components"][..],
            ),
        ] {
            let components = required_any(f, keys)?
                .as_array()
                .ok_or_else(|| BotError::Transaction("invalid components".into()))?;
            if components.is_empty() {
                return Err(BotError::Transaction("empty match components".into()));
            }
            for c in components {
                let (order, item) = fulfillment_component_indices(c)?;
                let (order, item) = (u256_to_usize(order)?, u256_to_usize(item)?);
                if order >= 2
                    || item >= sizes[order][side]
                    || !used.insert((order, side, item))
                    || (side == 1 && order == source)
                {
                    return Err(BotError::Transaction(
                        "duplicate, invalid, or self-routed match component".into(),
                    ));
                }
            }
        }
    }
    if used.len() != sizes.iter().map(|s| s[0] + s[1]).sum::<usize>() {
        return Err(BotError::Transaction(
            "match leaves order items unfulfilled".into(),
        ));
    }
    // The seller order uses direct Seaport approval. Do not honor an API-
    // supplied conduit that could spend a broader existing approval.
    let seller_view = match_advanced_order_view(&orders[seller_order])?;
    if required(seller_view.parameters, "conduitKey")?
        .as_str()
        .and_then(|s| s.parse::<B256>().ok())
        != Some(B256::ZERO)
    {
        return Err(BotError::Transaction(
            "seller match order must use the zero conduit key".into(),
        ));
    }
    Ok(())
}

fn validate_fulfillment_chain(
    fulfillment: &OpenSeaOfferFulfillment,
    expected_chain_id: u64,
) -> Result<()> {
    if fulfillment.transaction.chain != expected_chain_id {
        return Err(BotError::Transaction(format!(
            "OpenSea fulfillment targets chain ID {}, expected {}",
            fulfillment.transaction.chain, expected_chain_id
        )));
    }
    Ok(())
}

fn known_chain_slug(chain_id: u64) -> Option<&'static str> {
    match chain_id {
        crate::config::ROBINHOOD_MAINNET_CHAIN_ID => Some("robinhood"),
        crate::config::INK_MAINNET_CHAIN_ID => Some("ink"),
        crate::config::HYPEREVM_MAINNET_CHAIN_ID => Some("hyperevm"),
        crate::config::ABSTRACT_MAINNET_CHAIN_ID => Some("abstract"),
        1 => Some("ethereum"),
        8453 => Some("base"),
        137 => Some("matic"),
        10 => Some("optimism"),
        42161 => Some("arbitrum"),
        _ => None,
    }
}

/// Convert the decoded OpenSea fulfillment response into transaction calldata.
/// OpenSea returns structured Seaport arguments rather than hex calldata for
/// same-chain fulfillment, so this encoder supports the standard basic,
/// regular, advanced, and batched match-advanced Seaport paths.
pub fn encode_fulfillment(
    fulfillment: &OpenSeaOfferFulfillment,
    recipient: Address,
) -> Result<Vec<u8>> {
    let transaction = &fulfillment.transaction;
    let function = parse_function(&transaction.function)?;
    validate_fulfillment_function(&function)?;
    let lower = transaction.function.to_ascii_lowercase();
    let input = &transaction.input_data;
    let values = if lower.starts_with("fulfillbasicorder") {
        vec![basic_order_value(
            object_field(input, &["parameters", "basicOrderParameters"]).ok_or_else(|| {
                BotError::Transaction("basic fulfillment has no parameters".into())
            })?,
            function
                .inputs
                .first()
                .ok_or_else(|| BotError::Abi("basic fulfillment has no input".into()))?
                .resolve()
                .map_err(|err| BotError::Abi(err.to_string()))?,
        )?]
    } else if lower.starts_with("fulfilladvancedorder") {
        advanced_order_args(input, &function, recipient, fulfillment.orders.as_slice())?
    } else if lower.starts_with("fulfillorder") {
        regular_order_args(input, &function, fulfillment.orders.as_slice())?
    } else if lower.starts_with("matchadvancedorders") {
        match_advanced_order_args(input, &function, recipient, fulfillment.orders.as_slice())?
    } else {
        return Err(BotError::Transaction(format!(
            "OpenSea returned unsupported fulfillment function `{}`",
            transaction.function
        )));
    };
    function
        .abi_encode_input(&values)
        .map_err(|err| BotError::Abi(format!("{}: {err}", transaction.function)))
}

fn basic_order_value(value: &Value, _ty: alloy::dyn_abi::DynSolType) -> Result<DynSolValue> {
    let keys = [
        "considerationToken",
        "considerationIdentifier",
        "considerationAmount",
        "offerer",
        "zone",
        "offerToken",
        "offerIdentifier",
        "offerAmount",
        "basicOrderType",
        "startTime",
        "endTime",
        "zoneHash",
        "salt",
        "offererConduitKey",
        "fulfillerConduitKey",
        "totalOriginalAdditionalRecipients",
        "additionalRecipients",
        "signature",
    ];
    let object = value
        .as_object()
        .ok_or_else(|| BotError::Abi("basic order parameters are not an object".to_string()))?;
    let mut values = Vec::with_capacity(keys.len());
    values.push(json_address(required(object, keys[0])?)?);
    for key in &keys[1..3] {
        values.push(json_uint(required(object, key)?, 256)?);
    }
    values.push(json_address(required(object, keys[3])?)?);
    values.push(json_address(required(object, keys[4])?)?);
    values.push(json_address(required(object, keys[5])?)?);
    values.push(json_uint(required(object, keys[6])?, 256)?);
    values.push(json_uint(required(object, keys[7])?, 256)?);
    values.push(json_uint(required(object, keys[8])?, 8)?);
    values.push(json_uint(required(object, keys[9])?, 256)?);
    values.push(json_uint(required(object, keys[10])?, 256)?);
    values.push(json_fixed32(required(object, keys[11])?)?);
    values.push(json_uint(required(object, keys[12])?, 256)?);
    values.push(json_fixed32(required(object, keys[13])?)?);
    // The fulfiller conduit key is caller-controlled. Using the zero key lets
    // the bot rely on a direct approval to Seaport instead of guessing a
    // chain-specific conduit address.
    values.push(DynSolValue::FixedBytes(B256::ZERO, 32));
    values.push(json_uint(required(object, keys[15])?, 256)?);
    let recipients = required(object, keys[16])?
        .as_array()
        .ok_or_else(|| BotError::Abi("additionalRecipients is not an array".to_string()))?
        .iter()
        .map(|entry| {
            let entry = entry
                .as_object()
                .ok_or_else(|| BotError::Abi("additional recipient is not an object".into()))?;
            Ok(DynSolValue::Tuple(vec![
                json_uint(required(entry, "amount")?, 256)?,
                json_address(required(entry, "recipient")?)?,
            ]))
        })
        .collect::<Result<Vec<_>>>()?;
    values.push(DynSolValue::Array(recipients));
    values.push(json_bytes(required(object, keys[17])?)?);
    Ok(DynSolValue::Tuple(values))
}

fn regular_order_args(
    input: &Value,
    _function: &alloy::json_abi::Function,
    orders: &[Value],
) -> Result<Vec<DynSolValue>> {
    let order = object_field(input, &["order"])
        .ok_or_else(|| BotError::Abi("fulfillOrder has no order".to_string()))?;
    Ok(vec![
        order_value(order, orders.first())?,
        DynSolValue::FixedBytes(B256::ZERO, 32),
    ])
}

fn advanced_order_args(
    input: &Value,
    function: &alloy::json_abi::Function,
    recipient: Address,
    orders: &[Value],
) -> Result<Vec<DynSolValue>> {
    let order = object_field(input, &["advancedOrder", "advanced_order"])
        .ok_or_else(|| BotError::Abi("fulfillAdvancedOrder has no advanced order".to_string()))?;
    let criteria_values = object_field(input, &["criteriaResolvers", "criteria_resolvers"])
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let criteria = criteria_values
        .iter()
        .map(criteria_resolver_value)
        .collect::<Result<Vec<_>>>()?;
    let mut args = vec![
        advanced_order_value(order, orders.first())?,
        DynSolValue::Array(criteria),
        DynSolValue::FixedBytes(B256::ZERO, 32),
    ];
    // Seaport 1.4+ exposes the recipient argument; an older deployment used
    // a three-argument variant. Follow the function signature returned by
    // OpenSea instead of assuming one ABI shape.
    if function.inputs.len() >= 4 {
        args.push(DynSolValue::Address(recipient));
    }
    Ok(args)
}

fn match_advanced_order_args(
    input: &Value,
    function: &alloy::json_abi::Function,
    recipient: Address,
    orders_fallback: &[Value],
) -> Result<Vec<DynSolValue>> {
    let orders = match_order_values(input, orders_fallback)?;
    let advanced_orders = orders
        .iter()
        .enumerate()
        .map(|(index, order)| advanced_order_value(order, orders_fallback.get(index)))
        .collect::<Result<Vec<_>>>()?;
    let criteria = object_field(input, &["criteriaResolvers", "criteria_resolvers"])
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(criteria_resolver_value)
        .collect::<Result<Vec<_>>>()?;
    let (order_index_bits, item_index_bits) = match_fulfillment_component_bits(function)?;
    let fulfillments = object_field(input, &["fulfillments"])
        .and_then(Value::as_array)
        .ok_or_else(|| BotError::Abi("matchAdvancedOrders has no fulfillments".to_string()))?
        .iter()
        .map(|fulfillment| match_fulfillment_value(fulfillment, order_index_bits, item_index_bits))
        .collect::<Result<Vec<_>>>()?;
    let recipient = DynSolValue::Address(recipient);
    Ok(vec![
        DynSolValue::Array(advanced_orders),
        DynSolValue::Array(criteria),
        DynSolValue::Array(fulfillments),
        recipient,
    ])
}

fn match_fulfillment_component_bits(
    function: &alloy::json_abi::Function,
) -> Result<(usize, usize)> {
    let ty = function
        .inputs
        .get(2)
        .ok_or_else(|| BotError::Abi("matchAdvancedOrders has no fulfillment input".to_string()))?
        .resolve()
        .map_err(|err| BotError::Abi(err.to_string()))?;
    let tuple = match ty {
        DynSolType::Array(inner) => inner,
        _ => {
            return Err(BotError::Abi(
                "matchAdvancedOrders fulfillment input is not an array".to_string(),
            ));
        }
    };
    let fields = match tuple.as_ref() {
        DynSolType::Tuple(fields) if fields.len() == 2 => fields,
        _ => {
            return Err(BotError::Abi(
                "matchAdvancedOrders fulfillment input is not a tuple".to_string(),
            ));
        }
    };
    let component = match &fields[0] {
        DynSolType::Array(inner) => inner,
        _ => {
            return Err(BotError::Abi(
                "matchAdvancedOrders offer components are not an array".to_string(),
            ));
        }
    };
    let component_fields = match component.as_ref() {
        DynSolType::Tuple(fields) if fields.len() == 2 => fields,
        _ => {
            return Err(BotError::Abi(
                "matchAdvancedOrders components are not tuples".to_string(),
            ));
        }
    };
    let order_index_bits = match &component_fields[0] {
        DynSolType::Uint(bits) => *bits,
        _ => {
            return Err(BotError::Abi(
                "matchAdvancedOrders order index is not an unsigned integer".to_string(),
            ));
        }
    };
    let item_index_bits = match &component_fields[1] {
        DynSolType::Uint(bits) => *bits,
        _ => {
            return Err(BotError::Abi(
                "matchAdvancedOrders item index is not an unsigned integer".to_string(),
            ));
        }
    };
    Ok((order_index_bits, item_index_bits))
}

fn match_fulfillment_value(
    value: &Value,
    order_index_bits: usize,
    item_index_bits: usize,
) -> Result<DynSolValue> {
    let object = value
        .as_object()
        .ok_or_else(|| BotError::Abi("Seaport fulfillment is not an object".to_string()))?;
    let offer_components = required_any(object, &["offerComponents", "offer_components"])?
        .as_array()
        .ok_or_else(|| BotError::Abi("offerComponents is not an array".to_string()))?
        .iter()
        .map(|component| fulfillment_component_value(component, order_index_bits, item_index_bits))
        .collect::<Result<Vec<_>>>()?;
    let consideration_components = required_any(
        object,
        &["considerationComponents", "consideration_components"],
    )?
    .as_array()
    .ok_or_else(|| BotError::Abi("considerationComponents is not an array".to_string()))?
    .iter()
    .map(|component| fulfillment_component_value(component, order_index_bits, item_index_bits))
    .collect::<Result<Vec<_>>>()?;
    Ok(DynSolValue::Tuple(vec![
        DynSolValue::Array(offer_components),
        DynSolValue::Array(consideration_components),
    ]))
}

fn fulfillment_component_value(
    value: &Value,
    order_index_bits: usize,
    item_index_bits: usize,
) -> Result<DynSolValue> {
    let object = value.as_object().ok_or_else(|| {
        BotError::Abi("Seaport fulfillment component is not an object".to_string())
    })?;
    Ok(DynSolValue::Tuple(vec![
        json_uint(
            required_any(object, &["orderIndex", "order_index"])?,
            order_index_bits,
        )?,
        json_uint(
            required_any(object, &["itemIndex", "item_index"])?,
            item_index_bits,
        )?,
    ]))
}

fn order_value(value: &Value, fallback: Option<&Value>) -> Result<DynSolValue> {
    value
        .as_object()
        .ok_or_else(|| BotError::Abi("Seaport order is not an object".to_string()))?;
    let parameters = object_field(value, &["parameters"])
        .or(Some(value))
        .ok_or_else(|| BotError::Abi("Seaport order has no parameters".to_string()))?;
    let mut values = vec![order_parameters_value(parameters)?];
    let signature = object_field(value, &["signature"])
        .or_else(|| fallback.and_then(|value| object_field(value, &["signature"])))
        .ok_or_else(|| BotError::Abi("Seaport order has no signature".to_string()))?;
    values.push(json_bytes(signature)?);
    Ok(DynSolValue::Tuple(values))
}

fn advanced_order_value(value: &Value, fallback: Option<&Value>) -> Result<DynSolValue> {
    let object = value
        .as_object()
        .ok_or_else(|| BotError::Abi("Seaport advanced order is not an object".to_string()))?;
    // AdvancedOrder contains OrderParameters directly; its signature is a
    // sibling of numerator/denominator (unlike the separate Order tuple,
    // which is parameters plus signature). OpenSea has returned both
    // `{ order: {...} }` and the canonical `{ parameters: {...}, ... }` shape.
    let nested = object_field(value, &["order"]).unwrap_or(value);
    let parameters = object_field(nested, &["parameters"]).unwrap_or(nested);
    let signature = object_field(value, &["signature"])
        .or_else(|| object_field(nested, &["signature"]))
        .or_else(|| fallback.and_then(|value| object_field(value, &["signature"])))
        .ok_or_else(|| BotError::Abi("advanced order has no signature".to_string()))?;
    Ok(DynSolValue::Tuple(vec![
        order_parameters_value(parameters)?,
        json_uint(required(object, "numerator")?, 120)?,
        json_uint(required(object, "denominator")?, 120)?,
        json_bytes(signature)?,
        json_bytes(required(object, "extraData")?)?,
    ]))
}

fn order_parameters_value(value: &Value) -> Result<DynSolValue> {
    let object = value
        .as_object()
        .ok_or_else(|| BotError::Abi("order parameters are not an object".to_string()))?;
    let offer = required(object, "offer")?
        .as_array()
        .ok_or_else(|| BotError::Abi("order offer is not an array".to_string()))?
        .iter()
        .map(offer_item_value)
        .collect::<Result<Vec<_>>>()?;
    let consideration = required(object, "consideration")?
        .as_array()
        .ok_or_else(|| BotError::Abi("order consideration is not an array".to_string()))?
        .iter()
        .map(consideration_item_value)
        .collect::<Result<Vec<_>>>()?;
    Ok(DynSolValue::Tuple(vec![
        json_address(required(object, "offerer")?)?,
        json_address(required(object, "zone")?)?,
        DynSolValue::Array(offer),
        DynSolValue::Array(consideration),
        json_uint(required(object, "orderType")?, 8)?,
        json_uint(required(object, "startTime")?, 256)?,
        json_uint(required(object, "endTime")?, 256)?,
        json_fixed32(required(object, "zoneHash")?)?,
        json_uint(required(object, "salt")?, 256)?,
        json_fixed32(required(object, "conduitKey")?)?,
        json_uint(required(object, "totalOriginalConsiderationItems")?, 256)?,
    ]))
}

fn offer_item_value(value: &Value) -> Result<DynSolValue> {
    let object = value
        .as_object()
        .ok_or_else(|| BotError::Abi("offer item is not an object".into()))?;
    Ok(DynSolValue::Tuple(vec![
        json_uint(required(object, "itemType")?, 8)?,
        json_address(required(object, "token")?)?,
        json_uint(required(object, "identifierOrCriteria")?, 256)?,
        json_uint(required(object, "startAmount")?, 256)?,
        json_uint(required(object, "endAmount")?, 256)?,
    ]))
}

fn consideration_item_value(value: &Value) -> Result<DynSolValue> {
    let object = value
        .as_object()
        .ok_or_else(|| BotError::Abi("consideration item is not an object".into()))?;
    Ok(DynSolValue::Tuple(vec![
        json_uint(required(object, "itemType")?, 8)?,
        json_address(required(object, "token")?)?,
        json_uint(required(object, "identifierOrCriteria")?, 256)?,
        json_uint(required(object, "startAmount")?, 256)?,
        json_uint(required(object, "endAmount")?, 256)?,
        json_address(required(object, "recipient")?)?,
    ]))
}

fn criteria_resolver_value(value: &Value) -> Result<DynSolValue> {
    let object = value
        .as_object()
        .ok_or_else(|| BotError::Abi("criteria resolver is not an object".into()))?;
    let proof = object_field(value, &["criteriaProof", "criteria_proof"])
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .map(json_fixed32)
        .collect::<Result<Vec<_>>>()?;
    Ok(DynSolValue::Tuple(vec![
        json_uint(required_any(object, &["orderIndex", "order_index"])?, 256)?,
        json_uint(required_any(object, &["side"])?, 8)?,
        json_uint(
            required_any(object, &["index", "itemIndex", "item_index"])?,
            256,
        )?,
        json_uint(required_any(object, &["identifier"])?, 256)?,
        DynSolValue::Array(proof),
    ]))
}

fn object_field<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    keys.iter().find_map(|key| value.get(*key))
}

fn required<'a>(object: &'a Map<String, Value>, key: &str) -> Result<&'a Value> {
    object
        .get(key)
        .ok_or_else(|| BotError::Abi(format!("missing Seaport field `{key}`")))
}

fn required_any<'a>(object: &'a Map<String, Value>, keys: &[&str]) -> Result<&'a Value> {
    keys.iter()
        .find_map(|key| object.get(*key))
        .ok_or_else(|| BotError::Abi(format!("missing Seaport field `{}`", keys[0])))
}

fn json_address(value: &Value) -> Result<DynSolValue> {
    let value = value
        .as_str()
        .ok_or_else(|| BotError::Abi("address is not a string".to_string()))?;
    value
        .parse::<Address>()
        .map(DynSolValue::Address)
        .map_err(|_| BotError::Abi("invalid address in fulfillment data".to_string()))
}

fn json_uint(value: &Value, bits: usize) -> Result<DynSolValue> {
    let value = json_text(value)?
        .parse::<U256>()
        .map_err(|_| BotError::Abi("invalid integer in fulfillment data".to_string()))?;
    if value.bit_len() > bits {
        return Err(BotError::Abi(format!("integer does not fit uint{bits}")));
    }
    Ok(DynSolValue::Uint(value, bits))
}

fn json_fixed32(value: &Value) -> Result<DynSolValue> {
    let text = value
        .as_str()
        .ok_or_else(|| BotError::Abi("bytes32 is not a string".to_string()))?;
    let bytes = text
        .parse::<B256>()
        .map_err(|_| BotError::Abi("invalid bytes32 in fulfillment data".to_string()))?;
    Ok(DynSolValue::FixedBytes(bytes, 32))
}

fn json_bytes(value: &Value) -> Result<DynSolValue> {
    let text = value
        .as_str()
        .ok_or_else(|| BotError::Abi("bytes is not a string".to_string()))?;
    let text = text.strip_prefix("0x").unwrap_or(text);
    let bytes = hex::decode(text)
        .map_err(|_| BotError::Abi("invalid bytes in fulfillment data".to_string()))?;
    Ok(DynSolValue::Bytes(bytes))
}

fn json_text(value: &Value) -> Result<String> {
    match value {
        Value::String(value) => Ok(value.clone()),
        Value::Number(value) => Ok(value.to_string()),
        _ => Err(BotError::Abi(
            "numeric value is not a string or number".to_string(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::AutoSellConfig, opensea::OpenSeaOffer};
    use serde_json::json;

    #[test]
    fn hyperevm_cost_basis_uses_actual_opensea_payment_in_hype() {
        let config: MintConfig = serde_json::from_value(json!({
            "name": "HyperEVM drop",
            "chain_id": 999,
            "contract_address": "0x0000000000000000000000000000000000000001",
            "quantity": 2,
            "opensea_drop_slug": "hype-drop",
            "max_price_per_nft": "2",
            "mint": { "function": "mint(uint256)", "price_per_nft": "0" },
            "trigger": { "type": "block_timestamp", "timestamp": 0 }
        }))
        .unwrap();
        config.validate().unwrap();
        let receipt: TransactionReceipt = serde_json::from_value(json!({
            "transactionHash": B256::ZERO,
            "from": Address::ZERO,
            "gasUsed": "0x186a0",
            "effectiveGasPrice": "0x3b9aca00",
            "cumulativeGasUsed": "0x186a0",
            "status": "0x1",
            "logs": [],
            "logsBloom": format!("0x{}", "00".repeat(256)),
            "type": "0x2"
        }))
        .unwrap();
        let actual_payment = U256::from(3_000_000_000_000_000_000u128);
        let raw = raw_mint_cost_basis(&config, actual_payment, &receipt).unwrap();
        assert_eq!(expected_chain_slug(&config), Some("hyperevm"));
        assert_eq!(raw.mint_payment_currency, "HYPE");
        assert_eq!(raw.native_currency, "HYPE");
        assert_eq!(raw.mint_payment_amount, actual_payment);
        let oracle = PriceOracle::new().unwrap();
        let prices = PriceSnapshot::from_prices([("HYPE", U256::from(40_000_000))]);
        let costs = usd_cost_basis(&oracle, &prices, &raw).unwrap();
        assert_eq!(costs.mint_payment_usd, U256::from(120_000_000));
        assert_eq!(costs.mint_gas_usd, U256::from(4_000));
    }

    fn offer(currency: &str, value: u128, decimals: u8) -> OpenSeaOffer {
        OpenSeaOffer {
            order_hash: "0x1".to_string(),
            chain: "ethereum".to_string(),
            protocol_address: Address::ZERO,
            contract_address: Address::ZERO,
            token_id: U256::from(1),
            currency: currency.to_string(),
            value: U256::from(value),
            decimals,
            status: "ACTIVE".to_string(),
            remaining_quantity: 1,
        }
    }

    #[test]
    fn detects_erc721_transfer_to_wallet() {
        assert_eq!(
            event_topic("Transfer(address,address,uint256)"),
            keccak256("Transfer(address,address,uint256)")
        );
    }

    #[test]
    fn configured_eip1559_sell_fee_caps_are_honored_exactly() {
        let mut config: MintConfig = serde_json::from_value(json!({
            "name": "test",
            "chain_id": 1,
            "contract_address": "0x0000000000000000000000000000000000000000",
            "quantity": 1,
            "mint": { "function": "mint(uint256)" },
            "trigger": { "type": "manual" },
            "gas": {
                "mode": "manual",
                "max_fee_gwei": "12.5",
                "max_priority_fee_gwei": "1.25"
            }
        }))
        .unwrap();
        config.gas.multiplier = 1.5;
        let mut request = TransactionRequest::default();
        assert!(apply_configured_sell_fee_fields(&config, &mut request).unwrap());
        assert_eq!(request.max_fee_per_gas, Some(12_500_000_000));
        assert_eq!(request.max_priority_fee_per_gas, Some(1_250_000_000));
    }

    #[test]
    fn uses_weth_usd_price_for_offer_and_applies_fees() {
        let oracle = PriceOracle::new().unwrap();
        let config = MintConfig {
            name: "test".into(),
            chain_id: 1,
            native_currency: Some("ETH".into()),
            mint_payment_currency: None,
            mint_payment_decimals: 18,
            contract_address: format!("{:#x}", Address::ZERO),
            expected_contract_code_hash: None,
            opensea_drop_slug: None,
            opensea_execution_mode: crate::config::OpenSeaExecutionMode::Normal,
            require_zero_value: false,
            max_price_per_nft: None,
            quantity: 1,
            mint: crate::config::MintCallConfig {
                function: "mint(uint256)".into(),
                arguments: vec!["1".into()],
                proof: None,
                price_per_nft: "1".into(),
            },
            trigger: crate::config::MintTrigger::Manual,
            gas: crate::config::GasConfig::default(),
            nonce_strategy: crate::config::NonceStrategy::default(),
            replacement: crate::config::ReplacementConfig::default(),
            auto_sell: AutoSellConfig::default(),
            expected_start_time: None,
            confirmations: 1,
        };
        let prices = PriceSnapshot::from_prices([
            ("ETH", U256::from(2_000_000_000u64)),
            ("WETH", U256::from(2_000_000_000u64)),
        ]);
        let offer = offer("WETH", 1_500_000_000_000_000_000, 18);
        let result = calculate_profitability(
            &oracle,
            &prices,
            &config,
            CostBasis {
                mint_payment_usd: U256::from(1_000_000_000u64),
                mint_gas_usd: U256::ZERO,
            },
            &offer,
            FulfillmentPayout {
                gross_amount: offer.value,
                fee_amount: U256::from(75_000_000_000_000_000u128),
                seller_amount: U256::from(1_425_000_000_000_000_000u128),
            },
            U256::ZERO,
            U256::ZERO,
        )
        .unwrap();
        assert_eq!(result.gross_offer_usd, U256::from(3_000_000_000u64));
        assert_eq!(result.fee_usd, U256::from(150_000_000u64));
        assert_eq!(result.profit_usd, U256::from(1_850_000_000u64));
    }

    #[test]
    fn a_loss_never_passes_a_zero_profit_threshold() {
        let oracle = PriceOracle::new().unwrap();
        let config = MintConfig {
            name: "test".into(),
            chain_id: 1,
            native_currency: Some("ETH".into()),
            mint_payment_currency: None,
            mint_payment_decimals: 18,
            contract_address: format!("{:#x}", Address::ZERO),
            expected_contract_code_hash: None,
            opensea_drop_slug: None,
            opensea_execution_mode: crate::config::OpenSeaExecutionMode::Normal,
            require_zero_value: false,
            max_price_per_nft: None,
            quantity: 1,
            mint: crate::config::MintCallConfig {
                function: "mint(uint256)".into(),
                arguments: vec!["1".into()],
                proof: None,
                price_per_nft: "1".into(),
            },
            trigger: crate::config::MintTrigger::Manual,
            gas: crate::config::GasConfig::default(),
            nonce_strategy: crate::config::NonceStrategy::default(),
            replacement: crate::config::ReplacementConfig::default(),
            auto_sell: AutoSellConfig::default(),
            expected_start_time: None,
            confirmations: 1,
        };
        let prices = PriceSnapshot::from_prices([
            ("ETH", U256::from(2_000_000_000u64)),
            ("WETH", U256::from(2_000_000_000u64)),
        ]);
        let offer = offer("WETH", 400_000_000_000_000_000, 18);
        let result = calculate_profitability(
            &oracle,
            &prices,
            &config,
            CostBasis {
                mint_payment_usd: U256::from(1_000_000_000u64),
                mint_gas_usd: U256::ZERO,
            },
            &offer,
            FulfillmentPayout {
                gross_amount: offer.value,
                fee_amount: U256::ZERO,
                seller_amount: offer.value,
            },
            U256::ZERO,
            U256::ZERO,
        )
        .unwrap();
        assert!(!result.profitable);
        assert_eq!(result.profit_usd, U256::ZERO);
        assert_eq!(result.loss_usd, U256::from(200_000_000u64));
    }

    #[test]
    fn encodes_opensea_basic_offer_fulfillment() {
        let address = "0x0000000000000000000000000000000000000001";
        let zero32 = "0x0000000000000000000000000000000000000000000000000000000000000000";
        let input = json!({
            "parameters": {
                "considerationToken": "0x0000000000000000000000000000000000000000",
                "considerationIdentifier": "0",
                "considerationAmount": "1",
                "offerer": address,
                "zone": address,
                "offerToken": address,
                "offerIdentifier": "1",
                "offerAmount": "1",
                "basicOrderType": 5,
                "startTime": "1",
                "endTime": "2",
                "zoneHash": zero32,
                "salt": "3",
                "offererConduitKey": zero32,
                "fulfillerConduitKey": zero32,
                "totalOriginalAdditionalRecipients": "0",
                "additionalRecipients": [],
                "signature": "0x0102"
            }
        });
        let fulfillment = OpenSeaOfferFulfillment {
            protocol: "seaport1.6".into(),
            transaction: crate::opensea::OpenSeaFulfillmentTransaction {
                function: "fulfillBasicOrder_efficient_6GL6yc((address,uint256,uint256,address,address,address,uint256,uint256,uint8,uint256,uint256,bytes32,uint256,bytes32,bytes32,uint256,(uint256,address)[],bytes))".into(),
                chain: 1,
                to: Address::ZERO,
                value: U256::ZERO,
                input_data: input,
            },
            orders: Vec::new(),
        };
        let encoded = encode_fulfillment(&fulfillment, address.parse().unwrap()).unwrap();
        assert_eq!(&encoded[..4], &[0, 0, 0, 0]);
        assert!(encoded.len() > 4);
    }

    #[test]
    fn derives_basic_offer_fees_from_fulfillment_recipients() {
        let external_recipient = "0x0000000000000000000000000000000000000001";
        let seller: Address = "0x0000000000000000000000000000000000000002"
            .parse()
            .unwrap();
        let fulfillment = OpenSeaOfferFulfillment {
            protocol: "seaport1.6".into(),
            transaction: crate::opensea::OpenSeaFulfillmentTransaction {
                function: "fulfillBasicOrder((address))".into(),
                chain: 1,
                to: Address::ZERO,
                value: U256::ZERO,
                input_data: json!({
                    "parameters": {
                        "considerationToken": format!("{:#x}", Address::ZERO),
                        "considerationIdentifier": "1",
                        "offerAmount": "100",
                        "basicOrderType": 16,
                        "additionalRecipients": [
                            { "amount": "5", "recipient": external_recipient },
                            { "amount": "10", "recipient": format!("{seller:#x}") }
                        ]
                    }
                }),
            },
            orders: Vec::new(),
        };
        let payout = fulfillment_payout(&fulfillment, &offer("WETH", 100, 18), seller).unwrap();
        assert_eq!(payout.gross_amount, U256::from(100u64));
        assert_eq!(payout.fee_amount, U256::from(5u64));
        assert_eq!(payout.seller_amount, U256::from(95u64));
    }

    #[test]
    fn derives_advanced_offer_fees_and_validates_criteria_resolution() {
        let nft: Address = "0x0000000000000000000000000000000000000001"
            .parse()
            .unwrap();
        let seller: Address = "0x0000000000000000000000000000000000000002"
            .parse()
            .unwrap();
        let payment = "0x0000000000000000000000000000000000000009";
        let external = "0x0000000000000000000000000000000000000003";
        let buyer = "0x0000000000000000000000000000000000000004";
        let mut selected = offer("WETH", 1_000, 18);
        selected.contract_address = nft;
        selected.token_id = U256::from(123u64);
        let fulfillment = OpenSeaOfferFulfillment {
            protocol: "seaport1.6".into(),
            transaction: crate::opensea::OpenSeaFulfillmentTransaction {
                function: "fulfillAdvancedOrder((bytes))".into(),
                chain: 1,
                to: Address::ZERO,
                value: U256::ZERO,
                input_data: json!({
                    "advancedOrder": {
                        "parameters": {
                            "offer": [{
                                "itemType": 1,
                                "token": payment,
                                "identifierOrCriteria": "0",
                                "startAmount": "1000",
                                "endAmount": "1000"
                            }],
                            "consideration": [
                                {
                                    "itemType": 4,
                                    "token": format!("{nft:#x}"),
                                    "identifierOrCriteria": "0",
                                    "startAmount": "1",
                                    "endAmount": "1",
                                    "recipient": buyer
                                },
                                {
                                    "itemType": 1,
                                    "token": payment,
                                    "identifierOrCriteria": "0",
                                    "startAmount": "50",
                                    "endAmount": "50",
                                    "recipient": external
                                },
                                {
                                    "itemType": 1,
                                    "token": payment,
                                    "identifierOrCriteria": "0",
                                    "startAmount": "10",
                                    "endAmount": "10",
                                    "recipient": format!("{seller:#x}")
                                }
                            ]
                        },
                        "numerator": "1",
                        "denominator": "1"
                    },
                    "criteriaResolvers": [{
                        "orderIndex": "0",
                        "side": 1,
                        "index": "0",
                        "identifier": "123"
                    }]
                }),
            },
            orders: Vec::new(),
        };
        let payout = fulfillment_payout(&fulfillment, &selected, seller).unwrap();
        assert_eq!(payout.gross_amount, U256::from(1_000u64));
        assert_eq!(payout.fee_amount, U256::from(50u64));
        assert_eq!(payout.seller_amount, U256::from(950u64));
        let mut checked = fulfillment.clone();
        checked.transaction.to = SEAPORT;
        selected.protocol_address = SEAPORT;
        checked.transaction.function = format!(
            "fulfillAdvancedOrder(({ORDER_PARAMETERS},uint120,uint120,bytes,bytes),{RESOLVERS},bytes32,address)"
        );
        checked.transaction.input_data["advancedOrder"]["parameters"]["offerer"] = json!(buyer);
        validate_fulfillment_policy(&review_config(), &checked, &selected, seller).unwrap();
        for extra in [
            json!({"itemType":2,"token":nft,"identifierOrCriteria":"999","startAmount":"1","endAmount":"1","recipient":buyer}),
            json!({"itemType":1,"token":nft,"identifierOrCriteria":"0","startAmount":"10","endAmount":"10","recipient":buyer}),
        ] {
            let mut bad = checked.clone();
            bad.transaction.input_data["advancedOrder"]["parameters"]["consideration"]
                .as_array_mut()
                .unwrap()
                .push(extra);
            assert!(
                validate_fulfillment_policy(&review_config(), &bad, &selected, seller).is_err()
            );
        }
    }

    #[test]
    fn encodes_advanced_fulfillment_with_canonical_parameters_shape() {
        let address = "0x0000000000000000000000000000000000000001";
        let zero32 = "0x0000000000000000000000000000000000000000000000000000000000000000";
        let input = json!({
            "advancedOrder": {
                "parameters": {
                    "offerer": address,
                    "zone": address,
                    "offer": [{
                        "itemType": 1,
                        "token": address,
                        "identifierOrCriteria": "0",
                        "startAmount": "100",
                        "endAmount": "100"
                    }],
                    "consideration": [{
                        "itemType": 2,
                        "token": address,
                        "identifierOrCriteria": "1",
                        "startAmount": "1",
                        "endAmount": "1",
                        "recipient": address
                    }],
                    "orderType": 0,
                    "startTime": "1",
                    "endTime": "2",
                    "zoneHash": zero32,
                    "salt": "3",
                    "conduitKey": zero32,
                    "totalOriginalConsiderationItems": "1"
                },
                "numerator": "1",
                "denominator": "1",
                "signature": "0x0102",
                "extraData": "0x"
            },
            "criteriaResolvers": []
        });
        let fulfillment = OpenSeaOfferFulfillment {
            protocol: "seaport1.6".into(),
            transaction: crate::opensea::OpenSeaFulfillmentTransaction {
                function: "fulfillAdvancedOrder(((address,address,(uint8,address,uint256,uint256,uint256)[],(uint8,address,uint256,uint256,uint256,address)[],uint8,uint256,uint256,bytes32,uint256,bytes32,uint256),uint120,uint120,bytes,bytes),(uint256,uint8,uint256,uint256,bytes32[])[],bytes32,address)".into(),
                chain: 1,
                to: Address::ZERO,
                value: U256::ZERO,
                input_data: input,
            },
            orders: Vec::new(),
        };
        let encoded = encode_fulfillment(&fulfillment, address.parse().unwrap()).unwrap();
        assert!(encoded.len() > 4);
    }

    #[test]
    fn derives_and_encodes_match_advanced_orders_offer() {
        let nft: Address = "0x0000000000000000000000000000000000000001"
            .parse()
            .unwrap();
        let seller: Address = "0x0000000000000000000000000000000000000002"
            .parse()
            .unwrap();
        let external = "0x0000000000000000000000000000000000000003";
        let buyer = "0x0000000000000000000000000000000000000004";
        let payment = "0x0000000000000000000000000000000000000009";
        let zero32 = "0x0000000000000000000000000000000000000000000000000000000000000000";
        let mut selected = offer("WETH", 1_000, 18);
        selected.contract_address = nft;
        selected.token_id = U256::from(123u64);
        let order_parameters = |offer_items: Value, consideration: Value, total: u64| {
            json!({
                "offerer": buyer,
                "zone": format!("{seller:#x}"),
                "offer": offer_items,
                "consideration": consideration,
                "orderType": 0,
                "startTime": "1",
                "endTime": "2",
                "zoneHash": zero32,
                "salt": "3",
                "conduitKey": zero32,
                "totalOriginalConsiderationItems": total
            })
        };
        let input = json!({
            "orders": [
                {
                    "parameters": order_parameters(
                        json!([{
                            "itemType": 1,
                            "token": payment,
                            "identifierOrCriteria": "0",
                            "startAmount": "1000",
                            "endAmount": "1000"
                        }]),
                        json!([{
                            "itemType": 2,
                            "token": format!("{nft:#x}"),
                            "identifierOrCriteria": "0",
                            "startAmount": "1",
                            "endAmount": "1",
                            "recipient": buyer
                        }]),
                        1,
                    ),
                    "numerator": "1",
                    "denominator": "1",
                    "signature": "0x0102",
                    "extraData": "0x"
                },
                {
                    "parameters": order_parameters(
                        json!([{
                            "itemType": 4,
                            "token": format!("{nft:#x}"),
                            "identifierOrCriteria": "0",
                            "startAmount": "1",
                            "endAmount": "1"
                        }]),
                        json!([
                            {
                                "itemType": 1,
                                "token": payment,
                                "identifierOrCriteria": "0",
                                "startAmount": "50",
                                "endAmount": "50",
                                "recipient": external
                            },
                            {
                                "itemType": 1,
                                "token": payment,
                                "identifierOrCriteria": "0",
                                "startAmount": "950",
                                "endAmount": "950",
                                "recipient": format!("{seller:#x}")
                            }
                        ]),
                        2,
                    ),
                    "numerator": "1",
                    "denominator": "1",
                    "signature": "0x0304",
                    "extraData": "0x"
                }
            ],
            "criteriaResolvers": [{
                "orderIndex": "1",
                "side": 0,
                "index": "0",
                "identifier": "123",
                "criteriaProof": []
            }],
            "fulfillments": [
                {
                    "offerComponents": [{"orderIndex": "0", "itemIndex": "0"}],
                    "considerationComponents": [
                        {"orderIndex": "1", "itemIndex": "0"},
                        {"orderIndex": "1", "itemIndex": "1"}
                    ]
                },
                {
                    "offerComponents": [{"orderIndex": "1", "itemIndex": "0"}],
                    "considerationComponents": [{"orderIndex": "0", "itemIndex": "0"}]
                }
            ],
            "recipient": format!("{seller:#x}")
        });
        let fulfillment = OpenSeaOfferFulfillment {
            protocol: "seaport1.6".into(),
            transaction: crate::opensea::OpenSeaFulfillmentTransaction {
                function: "matchAdvancedOrders(((address,address,(uint8,address,uint256,uint256,uint256)[],(uint8,address,uint256,uint256,uint256,address)[],uint8,uint256,uint256,bytes32,uint256,bytes32,uint256),uint120,uint120,bytes,bytes)[],(uint256,uint8,uint256,uint256,bytes32[])[],((uint256,uint256)[],(uint256,uint256)[])[],address)".into(),
                chain: 1,
                to: Address::ZERO,
                value: U256::ZERO,
                input_data: input,
            },
            orders: Vec::new(),
        };
        let payout = fulfillment_payout(&fulfillment, &selected, seller).unwrap();
        assert_eq!(payout.gross_amount, U256::from(1_000u64));
        assert_eq!(payout.fee_amount, U256::from(50u64));
        assert_eq!(payout.seller_amount, U256::from(950u64));
        let encoded = encode_fulfillment(&fulfillment, seller).unwrap();
        assert!(encoded.len() > 4);
        let mut checked = fulfillment.clone();
        selected.protocol_address = SEAPORT;
        checked.transaction.to = SEAPORT;
        checked.transaction.input_data["orders"][1]["parameters"]["offerer"] = json!(seller);
        checked.transaction.input_data["orders"][0]["parameters"]["consideration"][0]["identifierOrCriteria"] =
            json!("123");
        validate_fulfillment_policy(&review_config(), &checked, &selected, seller).unwrap();
        let mut duplicate = checked.clone();
        let component =
            duplicate.transaction.input_data["fulfillments"][0]["considerationComponents"][0]
                .clone();
        duplicate.transaction.input_data["fulfillments"][0]["considerationComponents"]
            .as_array_mut()
            .unwrap()
            .push(component);
        assert!(
            validate_fulfillment_policy(&review_config(), &duplicate, &selected, seller).is_err()
        );
        let mut extra = checked.clone();
        let asset = extra.transaction.input_data["orders"][1]["parameters"]["offer"][0].clone();
        extra.transaction.input_data["orders"][1]["parameters"]["offer"]
            .as_array_mut()
            .unwrap()
            .push(asset);
        assert!(validate_fulfillment_policy(&review_config(), &extra, &selected, seller).is_err());
        let mut no_seller = checked.clone();
        no_seller.transaction.input_data["orders"][1]["parameters"]["offerer"] = json!(buyer);
        assert!(
            validate_fulfillment_policy(&review_config(), &no_seller, &selected, seller).is_err()
        );
        let mut wrong_abi = checked;
        wrong_abi.transaction.function = wrong_abi
            .transaction
            .function
            .replace("((uint256,uint256)[],", "((uint256,uint8)[],");
        assert!(encode_fulfillment(&wrong_abi, seller).is_err());
    }
    fn review_config() -> MintConfig {
        serde_json::from_value(json!({
            "name":"review", "chain_id":1,
            "contract_address":"0x0000000000000000000000000000000000000001",
            "quantity":1, "mint":{"function":"mint(uint256)"}, "trigger":{"type":"manual"},
            "auto_sell":{"currency_token_addresses":{"WETH":"0x0000000000000000000000000000000000000009"}}
        })).unwrap()
    }

    fn basic_fixture() -> (MintConfig, OpenSeaOffer, OpenSeaOfferFulfillment, Address) {
        let config = review_config();
        let seller = Address::repeat_byte(2);
        let mut offer = offer("WETH", 1000, 18);
        offer.contract_address = config.contract().unwrap();
        offer.protocol_address = SEAPORT;
        let fulfillment = OpenSeaOfferFulfillment {
            protocol: "seaport1.6".into(),
            orders: vec![],
            transaction: crate::opensea::OpenSeaFulfillmentTransaction {
                function: format!("fulfillBasicOrder({BASIC_PARAMETERS})"),
                chain: 1,
                to: SEAPORT,
                value: U256::ZERO,
                input_data: json!({"parameters":{
                    "considerationToken":config.contract_address,
                    "considerationIdentifier":"1", "considerationAmount":"1",
                    "offerer":Address::repeat_byte(3), "zone":Address::ZERO,
                    "offerToken":"0x0000000000000000000000000000000000000009",
                    "offerIdentifier":"0", "offerAmount":"1000", "basicOrderType":16,
                    "startTime":"1", "endTime":"2000000000", "zoneHash":B256::ZERO,
                    "salt":"1", "offererConduitKey":B256::ZERO, "fulfillerConduitKey":B256::ZERO,
                    "totalOriginalAdditionalRecipients":"1", "signature":"0x01",
                    "additionalRecipients":[{"amount":"50","recipient":Address::repeat_byte(4)}]
                }}),
            },
        };
        (config, offer, fulfillment, seller)
    }

    #[test]
    fn basic_sell_policy_rejects_asset_and_target_substitution() {
        let (config, offer, fulfillment, seller) = basic_fixture();
        validate_fulfillment_policy(&config, &fulfillment, &offer, seller).unwrap();
        assert!(!encode_fulfillment(&fulfillment, seller).unwrap().is_empty());
        for (field, value) in [
            ("offerToken", json!(Address::repeat_byte(9))),
            ("considerationToken", json!(Address::repeat_byte(9))),
            ("considerationIdentifier", json!("2")),
            ("considerationAmount", json!("2")),
            ("offerAmount", json!("999")),
            ("basicOrderType", json!(8)),
            ("offerer", json!(seller)),
        ] {
            let mut bad = fulfillment.clone();
            bad.transaction.input_data["parameters"][field] = value;
            assert!(
                validate_fulfillment_policy(&config, &bad, &offer, seller).is_err(),
                "{field}"
            );
        }
        let mut bad = fulfillment.clone();
        bad.transaction.to = Address::repeat_byte(8);
        assert!(validate_fulfillment_policy(&config, &bad, &offer, seller).is_err());
        bad = fulfillment.clone();
        bad.transaction.value = U256::from(1);
        assert!(validate_fulfillment_policy(&config, &bad, &offer, seller).is_err());
        bad = fulfillment.clone();
        bad.transaction.function =
            bad.transaction
                .function
                .replacen("fulfillBasicOrder", "fulfillBasicOrderMalicious", 1);
        assert!(encode_fulfillment(&bad, seller).is_err());
        let mut untrusted = config;
        untrusted.chain_id = 999;
        untrusted.auto_sell.currency_token_addresses.clear();
        bad = fulfillment;
        bad.transaction.chain = 999;
        assert!(validate_fulfillment_policy(&untrusted, &bad, &offer, seller).is_err());
    }

    fn receipt_with_logs(mut logs: Value) -> TransactionReceipt {
        for log in logs.as_array_mut().unwrap() {
            for field in [
                "blockNumber",
                "blockHash",
                "transactionHash",
                "transactionIndex",
                "logIndex",
            ] {
                log[field] = Value::Null;
            }
            log["removed"] = json!(false);
        }
        serde_json::from_value(json!({
            "transactionHash":B256::ZERO, "from":Address::ZERO,
            "gasUsed":"0x1", "effectiveGasPrice":"0x1", "cumulativeGasUsed":"0x1",
            "status":"0x1", "logs":logs, "logsBloom":format!("0x{}", "00".repeat(256)), "type":"0x2"
        }))
        .unwrap()
    }

    #[test]
    fn detects_actual_erc721_and_erc1155_recipients_and_rejects_zero_transfers() {
        let contract = Address::repeat_byte(1);
        let wallet = Address::repeat_byte(2);
        let wallet_topic = B256::from(wallet.into_word());
        let single = |from, to, amount: u64| {
            json!({"address":contract,
                "topics":[event_topic("TransferSingle(address,address,address,uint256,uint256)"), B256::ZERO, from, to],
                "data":format!("0x{:064x}{amount:064x}", 42)
            })
        };
        let incoming = receipt_with_logs(json!([single(B256::ZERO, wallet_topic, 1)]));
        assert_eq!(
            minted_token_ids(&incoming, contract, wallet),
            vec![U256::from(42)]
        );
        let outgoing = receipt_with_logs(json!([single(wallet_topic, B256::ZERO, 1)]));
        assert!(minted_token_ids(&outgoing, contract, wallet).is_empty());
        let zero = receipt_with_logs(json!([single(B256::ZERO, wallet_topic, 0)]));
        assert!(minted_token_ids(&zero, contract, wallet).is_empty());
        let erc721 = receipt_with_logs(json!([{"address":contract,
            "topics":[event_topic("Transfer(address,address,uint256)"), B256::ZERO, wallet_topic, B256::from(U256::from(7).to_be_bytes::<32>())],
            "data":"0x"}]));
        assert_eq!(
            minted_token_ids(&erc721, contract, wallet),
            vec![U256::from(7)]
        );
    }

    #[test]
    fn batch_log_decoding_is_bounded_by_available_data() {
        let words = [64, 160, 2, 42, 43, 2, 1, 0];
        let bytes: Vec<u8> = words
            .into_iter()
            .flat_map(|v| U256::from(v).to_be_bytes::<32>())
            .collect();
        assert_eq!(decode_batch_ids(&bytes), vec![U256::from(42)]);
        let mut bad = bytes.clone();
        bad[..32].fill(255);
        assert!(decode_batch_ids(&bad).is_empty());
        bad = bytes.clone();
        bad[64..96].fill(255);
        assert!(decode_batch_ids(&bad).is_empty());
        assert!(decode_batch_ids(&bytes[..bytes.len() - 1]).is_empty());
        bad = bytes;
        bad[160..192].copy_from_slice(&U256::from(1).to_be_bytes::<32>());
        assert!(decode_batch_ids(&bad).is_empty());
    }

    #[test]
    fn fulfillment_integers_cannot_exceed_their_abi_width() {
        assert!(json_uint(&json!(256), 8).is_err());
        assert!(json_uint(&json!(255), 8).is_ok());
        assert!(json_uint(&json!((U256::from(1) << 120u32).to_string()), 120).is_err());
    }

    #[test]
    fn sub_micro_dollar_expenses_cannot_pass_break_even() {
        let oracle = PriceOracle::new().unwrap();
        let config = review_config();
        let prices = PriceSnapshot::from_prices([
            ("ETH", U256::from(1_000_000)),
            ("WETH", U256::from(1_000_000)),
        ]);
        let offer = offer("WETH", 1, 18);
        let result = calculate_profitability(
            &oracle,
            &prices,
            &config,
            CostBasis {
                mint_payment_usd: U256::ZERO,
                mint_gas_usd: U256::ZERO,
            },
            &offer,
            FulfillmentPayout {
                gross_amount: U256::from(1),
                fee_amount: U256::ZERO,
                seller_amount: U256::from(1),
            },
            U256::from(2),
            U256::ZERO,
        )
        .unwrap();
        assert!(!result.profitable);
        assert_eq!(result.loss_usd, U256::from(1));
    }

    #[tokio::test]
    async fn ink_sell_budget_includes_oracle_surcharges() {
        let (rpc, server) = crate::rpc::tests::mock_rpc(|_| json!(format!("0x{:064x}", 10))).await;
        let mut config = review_config();
        config.chain_id = crate::config::INK_MAINNET_CHAIN_ID;
        let tx = TransactionRequest::default()
            .with_gas_limit(100)
            .with_gas_price(2)
            .with_input(vec![0; 10]);
        assert_eq!(
            transaction_gas_budget(&config, &rpc, &tx).await.unwrap(),
            U256::from(240)
        );
        server.abort();
    }
}
