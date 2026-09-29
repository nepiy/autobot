use crate::{
    error::{BotError, Result},
    security::validate_direct_mint_function,
};
use alloy::{
    json_abi::Function,
    primitives::{Address, B256, U256},
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fs, io::Write, path::Path};

pub const ROBINHOOD_MAINNET_CHAIN_ID: u64 = 4663;
pub const INK_MAINNET_CHAIN_ID: u64 = 57073;
pub const HYPEREVM_MAINNET_CHAIN_ID: u64 = 999;
pub const ABSTRACT_MAINNET_CHAIN_ID: u64 = 2741;
pub const ABSTRACT_DEFAULT_MAX_GAS_COST_NATIVE: &str = "0.001";
pub const ROBINHOOD_DEFAULT_GAS_LIMIT: u64 = 200_000;
pub const ROBINHOOD_DEFAULT_MAX_GAS_COST_NATIVE: &str = "0.001";
pub const INK_DEFAULT_GAS_LIMIT: u64 = 230_000;
pub const INK_DEFAULT_MAX_GAS_COST_NATIVE: &str = "0.001";
pub const HYPEREVM_DEFAULT_GAS_LIMIT: u64 = 230_000;
pub const HYPEREVM_DEFAULT_MAX_GAS_COST_NATIVE: &str = "0.001";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MintConfig {
    pub name: String,
    pub chain_id: u64,
    #[serde(default)]
    pub native_currency: Option<String>,
    /// Currency used to account for the native mint payment. If supplied, it
    /// must match the network's native currency; offers may use other tokens.
    #[serde(default)]
    pub mint_payment_currency: Option<String>,
    #[serde(default = "default_payment_decimals")]
    pub mint_payment_decimals: u8,
    pub contract_address: String,
    #[serde(default)]
    pub expected_contract_code_hash: Option<String>,
    #[serde(default)]
    pub opensea_drop_slug: Option<String>,
    #[serde(default)]
    pub opensea_execution_mode: OpenSeaExecutionMode,
    #[serde(default)]
    pub require_zero_value: bool,
    #[serde(default)]
    pub max_price_per_nft: Option<String>,
    pub quantity: u64,
    pub mint: MintCallConfig,
    pub trigger: MintTrigger,
    #[serde(default)]
    pub gas: GasConfig,
    #[serde(default)]
    pub nonce_strategy: NonceStrategy,
    #[serde(default)]
    pub replacement: ReplacementConfig,
    #[serde(default)]
    pub auto_sell: AutoSellConfig,
    #[serde(default)]
    pub expected_start_time: Option<u64>,
    #[serde(default = "default_confirmations")]
    pub confirmations: u64,
}

/// Controls how much OpenSea transaction preparation is performed on the
/// mint critical path. Both modes still obtain wallet-specific calldata from
/// OpenSea and enforce the configured payment guards.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OpenSeaExecutionMode {
    #[default]
    Normal,
    Aggressive,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AutoSellConfig {
    /// The post-mint OpenSea workflow is opt-in and disabled by default.
    #[serde(default)]
    pub enabled: bool,
    /// OpenSea collection slug. This is intentionally separate from a drop
    /// slug because a drop and its collection do not have to share a slug.
    #[serde(default)]
    pub collection_slug: Option<String>,
    /// OpenSea chain slug. Leave unset for chains with a built-in mapping;
    /// provide it for any other EVM network supported by OpenSea.
    #[serde(default)]
    pub opensea_chain: Option<String>,
    /// Minimum net profit, after royalties, marketplace fees, and gas.
    #[serde(default = "default_min_profit_usd")]
    pub min_profit_usd: String,
    /// Maximum time to wait for OpenSea indexing/offers after minting.
    #[serde(default = "default_offer_wait_seconds")]
    pub offer_wait_seconds: u64,
    /// Poll interval while waiting for an NFT to appear in OpenSea.
    #[serde(default = "default_offer_poll_seconds")]
    pub offer_poll_seconds: u64,
    /// Maximum time to monitor an approval or fulfillment transaction before
    /// abandoning the wait and continuing with the next token.
    #[serde(default = "default_auto_sell_receipt_timeout_seconds")]
    pub receipt_timeout_seconds: u64,
    /// Include optional creator fees when asking OpenSea to build fulfillment.
    #[serde(default)]
    pub include_optional_creator_fees: bool,
    /// Optional hard ceiling for the sell transaction's native gas cost.
    #[serde(default)]
    pub max_sell_gas_cost_native: Option<String>,
    /// If true, an unknown USD price causes the sale to be skipped.
    #[serde(default = "default_require_usd_price")]
    pub require_usd_price: bool,
    /// Explicit USD prices for currencies without a built-in live source.
    /// Keys are case-insensitive symbols such as USDG, POL, AVAX, or BNB.
    #[serde(default)]
    pub currency_usd_prices: BTreeMap<String, String>,
    /// Trusted ERC-20 contracts on this chain, keyed by offer currency symbol.
    /// A symbol or price supplied by an API is not a token identity.
    #[serde(default)]
    pub currency_token_addresses: BTreeMap<String, String>,
    /// Existing token used to exercise the complete OpenSea auto-sell path in
    /// `simulate` and `--dry-run` without broadcasting any transaction.
    #[serde(default)]
    pub dry_run_token_id: Option<String>,
}

impl Default for AutoSellConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            collection_slug: None,
            opensea_chain: None,
            min_profit_usd: default_min_profit_usd(),
            offer_wait_seconds: default_offer_wait_seconds(),
            offer_poll_seconds: default_offer_poll_seconds(),
            receipt_timeout_seconds: default_auto_sell_receipt_timeout_seconds(),
            include_optional_creator_fees: false,
            max_sell_gas_cost_native: None,
            require_usd_price: default_require_usd_price(),
            currency_usd_prices: BTreeMap::new(),
            currency_token_addresses: BTreeMap::new(),
            dry_run_token_id: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MintCallConfig {
    pub function: String,
    #[serde(default)]
    pub arguments: Vec<String>,
    #[serde(default)]
    pub proof: Option<Vec<String>>,
    #[serde(default = "default_price")]
    pub price_per_nft: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum MintTrigger {
    BlockTimestamp {
        timestamp: u64,
    },
    BooleanContractState {
        function: String,
        expected_value: bool,
    },
    NumericPhase {
        function: String,
        target_value: String,
    },
    ContractEvent {
        signature: String,
        #[serde(default)]
        confirmations: Option<u64>,
    },
    Manual,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum GasMode {
    #[default]
    Auto,
    Eip1559,
    Legacy,
    Manual,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GasConfig {
    #[serde(default)]
    pub mode: GasMode,
    #[serde(default = "default_multiplier")]
    pub multiplier: f64,
    pub gas_limit: Option<u64>,
    pub gas_price_gwei: Option<String>,
    pub max_fee_gwei: Option<String>,
    pub max_priority_fee_gwei: Option<String>,
    pub max_total_gas_cost_native: Option<String>,
}

impl Default for GasConfig {
    fn default() -> Self {
        Self {
            mode: GasMode::Auto,
            multiplier: default_multiplier(),
            gas_limit: None,
            gas_price_gwei: None,
            max_fee_gwei: None,
            max_priority_fee_gwei: None,
            max_total_gas_cost_native: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum NonceStrategy {
    #[default]
    Preloaded,
    RefreshEachBlock,
    JustBeforeTrigger,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplacementConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_after_blocks")]
    pub after_blocks: u64,
    #[serde(default = "default_replacement_multiplier")]
    pub fee_multiplier: f64,
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u32,
}

impl Default for ReplacementConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            after_blocks: default_after_blocks(),
            fee_multiplier: default_replacement_multiplier(),
            max_attempts: default_max_attempts(),
        }
    }
}

fn default_confirmations() -> u64 {
    1
}

fn default_payment_decimals() -> u8 {
    18
}

fn default_min_profit_usd() -> String {
    "0".to_string()
}

fn default_offer_wait_seconds() -> u64 {
    180
}

fn default_offer_poll_seconds() -> u64 {
    5
}

fn default_auto_sell_receipt_timeout_seconds() -> u64 {
    180
}

fn default_require_usd_price() -> bool {
    true
}

fn default_price() -> String {
    "0".to_string()
}

fn default_multiplier() -> f64 {
    1.15
}

fn default_after_blocks() -> u64 {
    2
}

fn default_replacement_multiplier() -> f64 {
    1.15
}

fn default_max_attempts() -> u32 {
    2
}

impl MintConfig {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let text = fs::read_to_string(path)
            .map_err(|err| BotError::Config(format!("could not read {}: {err}", path.display())))?;
        let config: Self = serde_json::from_str(&text)?;
        config.validate()?;
        Ok(config)
    }

    pub fn save_pretty(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        self.validate()?;
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        fs::create_dir_all(parent)?;
        // A private temporary file plus rename preserves the prior config if
        // serialization/writing fails, and never follows a destination symlink.
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        file.write_all((serde_json::to_string_pretty(self)? + "\n").as_bytes())?;
        file.as_file().sync_all()?;
        file.persist(path).map_err(|error| error.error)?;
        Ok(())
    }

    pub fn contract(&self) -> Result<Address> {
        self.contract_address
            .parse()
            .map_err(|_| BotError::InvalidAddress {
                value: self.contract_address.clone(),
            })
    }

    pub fn native_currency_symbol(&self) -> &str {
        self.native_currency
            .as_deref()
            .unwrap_or(match self.chain_id {
                HYPEREVM_MAINNET_CHAIN_ID => "HYPE",
                137 => "POL",
                _ => "ETH",
            })
    }

    pub fn expected_contract_code_hash_value(&self) -> Result<Option<B256>> {
        self.expected_contract_code_hash
            .as_deref()
            .map(|value| {
                if value.len() != 66
                    || !value.starts_with("0x")
                    || !value[2..].chars().all(|character| character.is_ascii_hexdigit())
                {
                    return Err(BotError::Config(format!(
                        "expected_contract_code_hash must be a 0x-prefixed 32-byte hash, got `{value}`"
                    )));
                }
                value
                    .parse::<B256>()
                    .map_err(|_| BotError::Config(format!(
                        "expected_contract_code_hash must be a 0x-prefixed 32-byte hash, got `{value}`"
                    )))
            })
            .transpose()
    }

    pub fn mint_value_wei(&self) -> Result<U256> {
        parse_native_amount(&self.mint.price_per_nft)?
            .checked_mul(U256::from(self.quantity))
            .ok_or_else(|| BotError::InvalidAmount {
                value: self.mint.price_per_nft.clone(),
                reason: "quantity multiplication overflowed".to_string(),
            })
    }

    pub fn maximum_opensea_mint_value_wei(&self) -> Result<Option<U256>> {
        self.max_price_per_nft
            .as_deref()
            .map(parse_native_amount)
            .transpose()?
            .map(|price| {
                price.checked_mul(U256::from(self.quantity)).ok_or_else(|| {
                    BotError::InvalidAmount {
                        value: self.max_price_per_nft.clone().unwrap_or_default(),
                        reason: "quantity multiplication overflowed".to_string(),
                    }
                })
            })
            .transpose()
    }

    pub fn validate(&self) -> Result<()> {
        if self.name.trim().is_empty() {
            return Err(BotError::Config("name must not be empty".to_string()));
        }
        if self.name.chars().any(char::is_control) {
            return Err(BotError::Config(
                "name must not contain terminal control characters".to_string(),
            ));
        }
        if let Some(currency) = self.native_currency.as_deref()
            && (currency.trim().is_empty()
                || !currency.chars().all(|character| {
                    character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
                }))
        {
            return Err(BotError::Config(
                "native_currency must be a currency symbol containing only letters, numbers, `-`, and `_`"
                    .to_string(),
            ));
        }
        if self.chain_id == 0 {
            return Err(BotError::Config(
                "chain_id must be greater than zero".to_string(),
            ));
        }
        let _ = self.contract()?;
        let _ = self.expected_contract_code_hash_value()?;
        if let Some(slug) = self.opensea_drop_slug.as_deref() {
            if slug.is_empty()
                || !slug.chars().all(|character| {
                    character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
                })
            {
                return Err(BotError::Config(
                    "opensea_drop_slug must contain only letters, numbers, `-`, and `_`"
                        .to_string(),
                ));
            }
            if !matches!(self.trigger, MintTrigger::BlockTimestamp { .. }) {
                return Err(BotError::Config(
                    "OpenSea mode requires a block_timestamp trigger".to_string(),
                ));
            }
            if self.quantity > 100 {
                return Err(BotError::Config(
                    "OpenSea mint quantity must be between 1 and 100".to_string(),
                ));
            }
            if matches!(
                self.opensea_execution_mode,
                OpenSeaExecutionMode::Aggressive
            ) && (self.gas.gas_limit.is_none() || self.gas.max_total_gas_cost_native.is_none())
            {
                return Err(BotError::Config(
                    "aggressive OpenSea mode requires gas.gas_limit and gas.max_total_gas_cost_native"
                        .to_string(),
                ));
            }
            let maximum = self.maximum_opensea_mint_value_wei()?;
            if self.require_zero_value {
                if maximum.is_some_and(|value| !value.is_zero()) {
                    return Err(BotError::Config(
                        "max_price_per_nft must be zero or omitted when require_zero_value is enabled"
                            .to_string(),
                    ));
                }
            } else if maximum.is_none() {
                return Err(BotError::Config(
                    "max_price_per_nft is required for a paid OpenSea mint".to_string(),
                ));
            }
        }
        if self.opensea_drop_slug.is_none()
            && !matches!(self.opensea_execution_mode, OpenSeaExecutionMode::Normal)
        {
            return Err(BotError::Config(
                "opensea_execution_mode requires opensea_drop_slug".to_string(),
            ));
        }
        if self.quantity == 0 {
            return Err(BotError::Config(
                "quantity must be greater than zero".to_string(),
            ));
        }
        if self.mint_payment_decimals != 18 {
            return Err(BotError::Config(
                "mint_payment_decimals must be 18 for native mint payments".to_string(),
            ));
        }
        if let Some(currency) = self.mint_payment_currency.as_deref()
            && currency.trim().is_empty()
        {
            return Err(BotError::Config(
                "mint_payment_currency must not be empty".to_string(),
            ));
        }
        if let Some(currency) = self.mint_payment_currency.as_deref()
            && !currency
                .trim()
                .eq_ignore_ascii_case(self.native_currency_symbol())
        {
            return Err(BotError::Config(
                "mint_payment_currency must match the native currency; ERC-20 mint payment approval is not part of this native-payable mint path"
                    .to_string(),
            ));
        }
        if self.mint.function.trim().is_empty() {
            return Err(BotError::Config(
                "mint.function must not be empty".to_string(),
            ));
        }
        if !self.mint.function.contains('(') {
            return Err(BotError::Config(
                "mint.function must be a Solidity signature such as mint(uint256)".to_string(),
            ));
        }
        let mint_function = Function::parse(&self.mint.function)
            .map_err(|error| BotError::Abi(format!("{}: {error}", self.mint.function)))?;
        if self.opensea_drop_slug.is_none() {
            validate_direct_mint_function(&mint_function)?;
        }
        let _ = parse_native_amount(&self.mint.price_per_nft)?;
        if !(self.gas.multiplier.is_finite() && self.gas.multiplier >= 1.0) {
            return Err(BotError::Config(
                "gas.multiplier must be finite and at least 1.0".to_string(),
            ));
        }
        if self.gas.gas_limit == Some(0) {
            return Err(BotError::Config(
                "gas.gas_limit must be greater than zero".to_string(),
            ));
        }
        if let Some(maximum) = self.gas.max_total_gas_cost_native.as_deref() {
            let _ = parse_native_amount(maximum)?;
        }
        match self.gas.mode {
            GasMode::Auto => {}
            GasMode::Legacy => {
                let value = self.gas.gas_price_gwei.as_deref().ok_or_else(|| {
                    BotError::Config("gas.gas_price_gwei is required for legacy mode".to_string())
                })?;
                let _ = parse_gwei(value)?;
            }
            GasMode::Eip1559 | GasMode::Manual => {
                let max_fee = self.gas.max_fee_gwei.as_deref().ok_or_else(|| {
                    BotError::Config(
                        "gas.max_fee_gwei is required for eip1559/manual mode".to_string(),
                    )
                })?;
                let priority = self.gas.max_priority_fee_gwei.as_deref().ok_or_else(|| {
                    BotError::Config(
                        "gas.max_priority_fee_gwei is required for eip1559/manual mode".to_string(),
                    )
                })?;
                if parse_gwei(priority)? > parse_gwei(max_fee)? {
                    return Err(BotError::Config(
                        "gas.max_priority_fee_gwei must not exceed max_fee_gwei".to_string(),
                    ));
                }
            }
        }
        if self.confirmations == 0 {
            return Err(BotError::Config(
                "confirmations must be greater than zero".to_string(),
            ));
        }
        if self.replacement.enabled {
            if self.replacement.max_attempts == 0 {
                return Err(BotError::Config(
                    "replacement.max_attempts must be greater than zero when enabled".to_string(),
                ));
            }
            if self.replacement.after_blocks == 0 {
                return Err(BotError::Config(
                    "replacement.after_blocks must be greater than zero when enabled".to_string(),
                ));
            }
            if !(self.replacement.fee_multiplier.is_finite()
                && self.replacement.fee_multiplier > 1.0)
            {
                return Err(BotError::Config(
                    "replacement.fee_multiplier must be finite and greater than 1.0".to_string(),
                ));
            }
        }
        if self.auto_sell.enabled {
            for (symbol, address) in &self.auto_sell.currency_token_addresses {
                if symbol.is_empty()
                    || !symbol
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
                    || !address
                        .parse::<Address>()
                        .is_ok_and(|address| !address.is_zero())
                {
                    return Err(BotError::Config("currency_token_addresses requires currency symbols and nonzero ERC-20 addresses".into()));
                }
            }
            for keys in [
                self.auto_sell.currency_token_addresses.keys(),
                self.auto_sell.currency_usd_prices.keys(),
            ] {
                let mut seen = std::collections::BTreeSet::new();
                if keys
                    .into_iter()
                    .any(|key| !seen.insert(key.to_ascii_uppercase()))
                {
                    return Err(BotError::Config(
                        "currency maps must not repeat symbols with different casing".into(),
                    ));
                }
            }
            let slug = self.auto_sell.collection_slug.as_deref().ok_or_else(|| {
                BotError::Config(
                    "auto_sell.collection_slug is required when auto_sell.enabled is true"
                        .to_string(),
                )
            })?;
            if slug.trim().is_empty()
                || !slug.chars().all(|character| {
                    character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
                })
            {
                return Err(BotError::Config(
                    "auto_sell.collection_slug must contain only letters, numbers, `-`, and `_`"
                        .to_string(),
                ));
            }
            if let Some(chain) = self.auto_sell.opensea_chain.as_deref()
                && (chain.trim().is_empty()
                    || !chain.chars().all(|character| {
                        character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
                    }))
            {
                return Err(BotError::Config(
                    "auto_sell.opensea_chain must contain only letters, numbers, `-`, and `_`"
                        .to_string(),
                ));
            }
            if self.auto_sell.opensea_chain.is_none()
                && !matches!(
                    self.chain_id,
                    ROBINHOOD_MAINNET_CHAIN_ID
                        | INK_MAINNET_CHAIN_ID
                        | HYPEREVM_MAINNET_CHAIN_ID
                        | ABSTRACT_MAINNET_CHAIN_ID
                        | 1
                        | 8453
                        | 137
                        | 10
                        | 42161
                )
            {
                return Err(BotError::Config(
                    "auto_sell.opensea_chain is required for an unmapped EVM chain".to_string(),
                ));
            }
            if self.auto_sell.offer_wait_seconds == 0 {
                return Err(BotError::Config(
                    "auto_sell.offer_wait_seconds must be greater than zero".to_string(),
                ));
            }
            if self.auto_sell.offer_poll_seconds == 0 {
                return Err(BotError::Config(
                    "auto_sell.offer_poll_seconds must be greater than zero".to_string(),
                ));
            }
            if self.auto_sell.receipt_timeout_seconds == 0 {
                return Err(BotError::Config(
                    "auto_sell.receipt_timeout_seconds must be greater than zero".to_string(),
                ));
            }
            let _ = parse_usd_amount(&self.auto_sell.min_profit_usd)?;
            if let Some(maximum) = self.auto_sell.max_sell_gas_cost_native.as_deref() {
                let _ = parse_native_amount(maximum)?;
            }
            for (currency, price) in &self.auto_sell.currency_usd_prices {
                if currency.trim().is_empty()
                    || !currency.chars().all(|character| {
                        character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
                    })
                {
                    return Err(BotError::Config(
                        "auto_sell.currency_usd_prices keys must be currency symbols containing only letters, numbers, `-`, and `_`"
                            .to_string(),
                    ));
                }
                if parse_usd_amount(price)?.is_zero() {
                    return Err(BotError::Config(format!(
                        "auto_sell.currency_usd_prices.{currency} must be greater than zero"
                    )));
                }
            }
            if let Some(token_id) = self.auto_sell.dry_run_token_id.as_deref()
                && (token_id.is_empty()
                    || !token_id.chars().all(|character| character.is_ascii_digit())
                    || token_id.parse::<U256>().is_err())
            {
                return Err(BotError::Config(
                    "auto_sell.dry_run_token_id must be a non-negative base-10 integer".to_string(),
                ));
            }
        }
        let _ = crate::trigger::TriggerEngine::new(self)?;
        Ok(())
    }
}

/// Parse a non-negative decimal USD amount into micro-dollars. Keeping money
/// as integers avoids floating point rounding in the sell decision.
pub fn parse_usd_amount(value: &str) -> Result<U256> {
    parse_decimal_units(value, 6)
}

pub fn parse_native_amount(value: &str) -> Result<U256> {
    let value = value.trim();
    if value.is_empty() {
        return Err(BotError::InvalidAmount {
            value: value.to_string(),
            reason: "empty value".to_string(),
        });
    }
    let mut parts = value.split('.');
    let whole = parts.next().unwrap_or_default();
    let fraction = parts.next().unwrap_or_default();
    if parts.next().is_some() || whole.is_empty() || !whole.chars().all(|c| c.is_ascii_digit()) {
        return Err(BotError::InvalidAmount {
            value: value.to_string(),
            reason: "expected a non-negative decimal number".to_string(),
        });
    }
    if fraction.len() > 18 || !fraction.chars().all(|c| c.is_ascii_digit()) {
        return Err(BotError::InvalidAmount {
            value: value.to_string(),
            reason: "native amounts support at most 18 decimal places".to_string(),
        });
    }
    let whole = U256::from_str_radix(whole, 10).map_err(|err| BotError::InvalidAmount {
        value: value.to_string(),
        reason: err.to_string(),
    })?;
    let fraction_padded = format!("{fraction:0<18}");
    let fraction =
        U256::from_str_radix(&fraction_padded, 10).map_err(|err| BotError::InvalidAmount {
            value: value.to_string(),
            reason: err.to_string(),
        })?;
    whole
        .checked_mul(U256::from(1_000_000_000_000_000_000u128))
        .and_then(|base| base.checked_add(fraction))
        .ok_or_else(|| BotError::InvalidAmount {
            value: value.to_string(),
            reason: "value overflowed U256".to_string(),
        })
}

pub fn parse_gwei(value: &str) -> Result<u128> {
    let wei = parse_decimal_units(value, 9)?;
    if wei > U256::from(u128::MAX) {
        return Err(BotError::InvalidAmount {
            value: value.to_string(),
            reason: "gwei value does not fit into u128 wei".to_string(),
        });
    }
    Ok(wei.to::<u128>())
}

fn parse_decimal_units(value: &str, decimals: usize) -> Result<U256> {
    let value = value.trim();
    let mut parts = value.split('.');
    let whole = parts.next().unwrap_or_default();
    let fraction = parts.next().unwrap_or_default();
    if parts.next().is_some()
        || whole.is_empty()
        || !whole.chars().all(|c| c.is_ascii_digit())
        || fraction.len() > decimals
        || !fraction.chars().all(|c| c.is_ascii_digit())
    {
        return Err(BotError::InvalidAmount {
            value: value.to_string(),
            reason: format!("expected a decimal with at most {decimals} places"),
        });
    }
    let whole = U256::from_str_radix(whole, 10).map_err(|err| BotError::InvalidAmount {
        value: value.to_string(),
        reason: err.to_string(),
    })?;
    let scale = U256::from(10u64).pow(U256::from(decimals));
    let fraction = format!("{fraction:0<decimals$}");
    let fraction = U256::from_str_radix(&fraction, 10).map_err(|err| BotError::InvalidAmount {
        value: value.to_string(),
        reason: err.to_string(),
    })?;
    whole
        .checked_mul(scale)
        .and_then(|base| base.checked_add(fraction))
        .ok_or_else(|| BotError::InvalidAmount {
            value: value.to_string(),
            reason: "value overflowed U256".to_string(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_native_amounts() {
        assert_eq!(
            parse_native_amount("0.005").unwrap(),
            U256::from(5_000_000_000_000_000u64)
        );
        assert_eq!(
            parse_native_amount("1").unwrap(),
            U256::from(1_000_000_000_000_000_000u64)
        );
    }

    #[test]
    fn rejects_more_than_eighteen_decimals() {
        assert!(parse_native_amount("0.0000000000000000001").is_err());
    }

    #[test]
    fn parses_usd_micro_dollars() {
        assert_eq!(
            parse_usd_amount("12.345678").unwrap(),
            U256::from(12_345_678)
        );
        assert!(parse_usd_amount("12.3456789").is_err());
    }
}
