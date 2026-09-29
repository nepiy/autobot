use super::*;
use crate::{opensea::OpenSeaFulfillmentTransaction, pricing::PriceSnapshot};
use serde_json::json;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
};

const BASIC: &str = "fulfillBasicOrder((address,uint256,uint256,address,address,address,uint256,uint256,uint8,uint256,uint256,bytes32,uint256,bytes32,bytes32,uint256,(uint256,address)[],bytes))";
fn config(chain_id: u64) -> AutoBuyConfig {
    serde_json::from_value(json!({
        "chain_id": chain_id, "contract_address": Address::repeat_byte(1),
        "target_price_usd": "50", "price_tolerance_percent": "10", "quantity": 2,
        "poll_seconds": 1, "receipt_timeout_seconds": 1, "confirmations": 2
    }))
    .unwrap()
}
fn listing_json(config: &AutoBuyConfig, id: u64) -> Value {
    json!({
        "chain": opensea_chain_slug(config.chain_id).unwrap(), "status": "ACTIVE",
        "remaining_quantity": 1, "protocol_address": SEAPORT,
        "order_hash": B256::repeat_byte(id as u8),
        "price": {"current": {"currency": config.native_symbol(), "decimals": 18, "value": "25000000000000000"}},
        "protocol_data": {"parameters": {"offerer": Address::repeat_byte(3),
            "offer": [{"itemType": 2, "token": config.contract_address, "identifierOrCriteria": id.to_string(), "startAmount": "1", "endAmount": "1"}],
            "consideration": [{"itemType": 0, "token": Address::ZERO, "identifierOrCriteria": "0", "startAmount": "25000000000000000", "endAmount": "25000000000000000", "recipient": Address::repeat_byte(3)}]
        }}
    })
}
fn fulfillment(config: &AutoBuyConfig, id: u64) -> OpenSeaOfferFulfillment {
    OpenSeaOfferFulfillment {
        protocol: "seaport1.6".into(),
        orders: vec![],
        transaction: OpenSeaFulfillmentTransaction {
            function: BASIC.into(),
            chain: config.chain_id,
            to: SEAPORT,
            value: parse_native_amount("0.025").unwrap(),
            input_data: json!({"parameters": {
                "considerationToken": Address::ZERO, "considerationIdentifier": "0", "considerationAmount": "24000000000000000",
                "offerer": Address::repeat_byte(3), "zone": Address::ZERO,
                "offerToken": config.contract_address, "offerIdentifier": id.to_string(), "offerAmount": "1", "basicOrderType": 0,
                "startTime": "1", "endTime": "9999999999", "zoneHash": B256::ZERO, "salt": "1",
                "offererConduitKey": B256::ZERO, "fulfillerConduitKey": B256::ZERO,
                "totalOriginalAdditionalRecipients": "1", "additionalRecipients": [{"amount":"1000000000000000", "recipient": Address::repeat_byte(4)}], "signature": "0x01"
            }}),
        },
    }
}

#[test]
fn symmetric_price_band_includes_both_edges_and_rejects_invalid_settings() {
    let c = config(2741);
    for (price, expected) in [
        ("44.999999", false),
        ("45", true),
        ("50", true),
        ("55", true),
        ("55.000001", false),
    ] {
        assert_eq!(
            c.in_price_band(parse_usd_amount(price).unwrap()).unwrap(),
            expected
        );
    }
    for tolerance in ["-1", "100.01", "NaN", "0.001"] {
        let mut c = c.clone();
        c.price_tolerance_percent = tolerance.into();
        assert!(c.validate().is_err());
    }
    let mut exact = c.clone();
    exact.price_tolerance_percent = "0".into();
    assert_eq!(
        exact.price_band().unwrap(),
        (
            parse_usd_amount("50").unwrap(),
            parse_usd_amount("50").unwrap()
        )
    );
    let mut bad = c.clone();
    bad.quantity = 0;
    assert!(bad.validate().is_err());
    bad = c.clone();
    bad.chain_id = 1;
    assert!(bad.validate().is_err());
    bad = c.clone();
    bad.target_price_usd = "0".into();
    assert!(bad.validate().is_err());
    bad = c;
    bad.target_price_usd = U256::MAX.to_string();
    assert!(bad.validate().is_err());
}

#[test]
fn listing_validation_rejects_wrong_assets_spoofed_native_currency_and_bundles() {
    let c = config(4663);
    let buyer = Address::repeat_byte(2);
    let good = listing_json(&c, 1);
    assert_eq!(
        parse_listing(&good, &c, buyer).unwrap().token_id,
        U256::from(1)
    );
    for (path, value) in [
        ("/chain", json!("ink")),
        ("/status", json!("FULFILLED")),
        ("/protocol_address", json!(Address::repeat_byte(5))),
        ("/protocol_data/parameters/offerer", json!(buyer)),
        (
            "/protocol_data/parameters/offer/0/token",
            json!(Address::repeat_byte(5)),
        ),
        (
            "/protocol_data/parameters/consideration/0/itemType",
            json!(1),
        ),
        (
            "/protocol_data/parameters/consideration/0/token",
            json!(Address::repeat_byte(5)),
        ),
        (
            "/protocol_data/parameters/consideration/0/endAmount",
            json!("99"),
        ),
        ("/price/current/currency", json!("WETH")),
    ] {
        let mut v = good.clone();
        *v.pointer_mut(path).unwrap() = value;
        assert!(parse_listing(&v, &c, buyer).is_err(), "{path}");
    }
    let mut bundle = good;
    let nft = bundle["protocol_data"]["parameters"]["offer"][0].clone();
    bundle["protocol_data"]["parameters"]["offer"]
        .as_array_mut()
        .unwrap()
        .push(nft);
    assert!(parse_listing(&bundle, &c, buyer).is_err());
}

#[test]
fn validates_exact_basic_fulfillment_and_all_required_fees() {
    let c = config(999);
    let buyer = Address::repeat_byte(2);
    let selected = parse_listing(&listing_json(&c, 1), &c, buyer).unwrap();
    let good = fulfillment(&c, 1);
    assert!(
        !validate_fulfillment(&good, &selected, &c, buyer, 100)
            .unwrap()
            .is_empty()
    );
    for (key, value) in [
        ("offerToken", json!(buyer)),
        ("offerIdentifier", json!("2")),
        ("offerAmount", json!("2")),
        ("basicOrderType", json!(8)),
        ("considerationToken", json!(buyer)),
        ("offerer", json!(buyer)),
        ("startTime", json!("101")),
        ("endTime", json!("100")),
    ] {
        let mut f = good.clone();
        f.transaction.input_data["parameters"][key] = value;
        assert!(
            validate_fulfillment(&f, &selected, &c, buyer, 100).is_err(),
            "{key}"
        );
    }
    let mut f = good.clone();
    f.transaction.value += U256::from(1);
    assert!(validate_fulfillment(&f, &selected, &c, buyer, 100).is_err());
    let mut f = good.clone();
    f.transaction.function = "fulfillBasicOrder((address))".into();
    assert!(validate_fulfillment(&f, &selected, &c, buyer, 100).is_err());
    let mut f = good.clone();
    f.transaction.chain = 4663;
    assert!(validate_fulfillment(&f, &selected, &c, buyer, 100).is_err());
    let mut f = good;
    f.transaction.to = buyer;
    assert!(validate_fulfillment(&f, &selected, &c, buyer, 100).is_err());
}

#[test]
fn progress_survives_restart_and_changed_threshold_without_counting_reverts() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("progress.json");
    let mut p = BuyProgress::default();
    let pending = PendingBuy {
        hash: B256::repeat_byte(9),
        order_hash: B256::repeat_byte(1),
        token_id: U256::from(1),
        item_type: 2,
        raw_transaction: None,
        nonce: None,
        broadcast_attempted: true,
    };
    p.pending = Some(pending.clone());
    p.save(&path).unwrap();
    let mut p = BuyProgress::load(&path).unwrap();
    assert_eq!(p.pending.as_ref().unwrap().hash, pending.hash);
    p.finish(false).unwrap();
    assert_eq!(p.purchased, 0);
    p.pending = Some(pending);
    p.finish(true).unwrap();
    p.save(&path).unwrap();
    let p = BuyProgress::load(&path).unwrap();
    assert_eq!(p.purchased, 1);
    assert!(p.purchased_erc721.contains(&U256::from(1)));
    let c = config(2741);
    let mut changed = c.clone();
    changed.target_price_usd = "60".into();
    changed.quantity = 3;
    assert_eq!(
        progress_path(&c, Address::ZERO),
        progress_path(&changed, Address::ZERO)
    );
    changed.session = "another".into();
    assert_ne!(
        progress_path(&c, Address::ZERO),
        progress_path(&changed, Address::ZERO)
    );
}

async fn mock_opensea(
    c: AutoBuyConfig,
    wrong_price: bool,
) -> (OpenSeaClient, tokio::task::JoinHandle<()>) {
    mock_opensea_responder(move |route, request| opensea_response(&c, wrong_price, route, request))
        .await
}

fn opensea_response(c: &AutoBuyConfig, wrong_price: bool, route: &str, request: &Value) -> Value {
    if route.starts_with("GET /listings/collection/test/best?") {
        json!({"listings": [listing_json(c, 1), listing_json(c, 2), listing_json(c, 3)]})
    } else if route.starts_with("POST /listings/fulfillment_data ") {
        assert_eq!(request["units_to_fill"], 1);
        assert_eq!(
            request["listing"]["chain"],
            opensea_chain_slug(c.chain_id).unwrap()
        );
        let hash: B256 = serde_json::from_value(request["listing"]["hash"].clone()).unwrap();
        let id = hash.as_slice()[0] as u64;
        let mut f = fulfillment(c, id);
        if wrong_price {
            f.transaction.value = parse_native_amount("0.028").unwrap();
            f.transaction.input_data["parameters"]["considerationAmount"] =
                json!("27000000000000000");
        }
        json!({"protocol": f.protocol, "fulfillment_data": {"orders": [], "transaction": {
            "function": f.transaction.function, "chain": f.transaction.chain, "to": f.transaction.to,
            "value": f.transaction.value.to_string(), "input_data": f.transaction.input_data
        }}})
    } else {
        panic!("unexpected OpenSea route {route}")
    }
}

async fn mock_opensea_responder<F>(mut respond: F) -> (OpenSeaClient, tokio::task::JoinHandle<()>)
where
    F: FnMut(&str, &Value) -> Value + Send + 'static,
{
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        loop {
            let (socket, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(socket);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            let route = line.clone();
            let mut length = 0;
            loop {
                line.clear();
                reader.read_line(&mut line).await.unwrap();
                if line == "\r\n" {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse::<usize>().unwrap();
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).await.unwrap();
            let request: Value = if body.is_empty() {
                Value::Null
            } else {
                serde_json::from_slice(&body).unwrap()
            };
            let response = respond(&route, &request);
            let body = response.to_string();
            reader.get_mut().write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
        }
    });
    (OpenSeaClient::for_buy_test(base), server)
}

fn receipt_json(c: &AutoBuyConfig, buyer: Address, hash: B256, id: u64, success: bool) -> Value {
    let receipt = json!({
        "transactionHash": hash, "transactionIndex": "0x0", "blockNumber": "0x1", "blockHash": B256::repeat_byte(7),
        "from": buyer, "to": SEAPORT, "gasUsed": "0x249f0", "effectiveGasPrice": "0x1", "cumulativeGasUsed": "0x249f0",
        "status": if success {"0x1"} else {"0x0"}, "logsBloom": format!("0x{}", "00".repeat(256)), "type":"0x2",
        "logs": if success {vec![json!({"address": c.contract_address, "topics": [keccak256("Transfer(address,address,uint256)"), B256::from(Address::repeat_byte(3).into_word()), B256::from(buyer.into_word()), B256::from(U256::from(id).to_be_bytes::<32>())], "data":"0x", "removed": false, "blockNumber":"0x1", "blockHash":B256::repeat_byte(7), "transactionHash":hash, "transactionIndex":"0x0", "logIndex":"0x0", "blockTimestamp":"0x64"})]} else {vec![]}
    });
    serde_json::from_value::<TransactionReceipt>(receipt.clone()).expect("valid mock receipt");
    receipt
}

struct Fixture {
    config: AutoBuyConfig,
    wallet: LoadedWallet,
    client: OpenSeaClient,
    rpc: RpcClients,
    oracle: PriceOracle,
    dir: tempfile::TempDir,
    path: PathBuf,
    sends: Arc<AtomicUsize>,
    receipt_ready: Arc<AtomicBool>,
    revert_all: Arc<AtomicBool>,
    reject_broadcast: Arc<AtomicBool>,
    ambiguous_broadcast: Arc<AtomicBool>,
    receipt_gas_price: Arc<AtomicU64>,
    broadcast_calls: Arc<AtomicUsize>,
    servers: Vec<tokio::task::JoinHandle<()>>,
}
impl Fixture {
    async fn new(chain: u64, revert_first: bool, wrong_price: bool) -> Self {
        let c = config(chain);
        let signer = alloy::signers::local::PrivateKeySigner::random();
        let wallet = LoadedWallet {
            address: signer.address(),
            wallet: alloy::network::EthereumWallet::new(signer),
        };
        let (client, opensea_server) = mock_opensea(c.clone(), wrong_price).await;
        let sends = Arc::new(AtomicUsize::new(0));
        let sent = sends.clone();
        let receipt_ready = Arc::new(AtomicBool::new(true));
        let ready = receipt_ready.clone();
        let revert_all = Arc::new(AtomicBool::new(false));
        let all_revert = revert_all.clone();
        let reject_broadcast = Arc::new(AtomicBool::new(false));
        let reject = reject_broadcast.clone();
        let ambiguous_broadcast = Arc::new(AtomicBool::new(false));
        let ambiguous = ambiguous_broadcast.clone();
        let receipt_gas_price = Arc::new(AtomicU64::new(1));
        let gas_price = receipt_gas_price.clone();
        let broadcast_calls = Arc::new(AtomicUsize::new(0));
        let calls = broadcast_calls.clone();
        let receipts = Arc::new(Mutex::new(std::collections::BTreeMap::new()));
        let copy = c.clone();
        let buyer = wallet.address;
        let (rpc, server) = crate::rpc::tests::mock_rpc_async(move |request| {
            let result = match request["method"].as_str().unwrap() {
                "eth_getTransactionCount" => json!(format!("0x{:x}", sent.load(Ordering::SeqCst))),
                "eth_getBalance" => json!("0xde0b6b3a7640000"),
                "eth_feeHistory" => json!({"oldestBlock":"0x1", "baseFeePerGas":["0x64","0x64"], "gasUsedRatio":[0.5], "reward":[["0xa"]]}),
                "eth_estimateGas" => json!("0x249f0"),
                "eth_call" => {
                    let tx = &request["params"][0];
                    if tx["to"] == json!(address!("420000000000000000000000000000000000000F")) { json!(format!("0x{:064x}", 1)) } else {json!("0x")}
                },
                "eth_blockNumber" => json!("0x2"),
                "eth_getBlockByNumber" => {
                    let mut block = alloy::rpc::types::Block::<alloy::rpc::types::Transaction>::default();
                    block.header.inner.timestamp = 100; serde_json::to_value(block).unwrap()
                },
                "eth_sendRawTransaction" => {
                    calls.fetch_add(1, Ordering::SeqCst);
                    if reject.load(Ordering::SeqCst) { return std::future::ready(json!({"__mock_rpc_error":{"code":-32000,"message":"transaction underpriced"}})); }
                    if ambiguous.load(Ordering::SeqCst) { return std::future::ready(json!("invalid acknowledgement")); }
                    let raw = hex::decode(request["params"][0].as_str().unwrap().trim_start_matches("0x")).unwrap();
                    let hash = keccak256(&raw);
                    if receipts.lock().unwrap().contains_key(&hash) { return std::future::ready(json!(hash)); }
                    let index = sent.fetch_add(1, Ordering::SeqCst);
                    let success = (!revert_first || index > 0) && !all_revert.load(Ordering::SeqCst);
                    let tx = TxEnvelope::decode_2718(&mut raw.as_slice()).unwrap();
                    use alloy::dyn_abi::JsonAbiExt;
                    let decoded = crate::abi::parse_function(BASIC).unwrap().abi_decode_input(&tx.input()[4..]).unwrap();
                    let alloy::dyn_abi::DynSolValue::Tuple(params) = &decoded[0] else { panic!("not a basic order") };
                    let alloy::dyn_abi::DynSolValue::Uint(id, _) = params[6] else { panic!("missing token id") };
                    let token_id = id.to::<u64>();
                    let mut receipt = receipt_json(&copy, buyer, hash, token_id, success);
                    receipt["effectiveGasPrice"] = json!(format!("0x{:x}", gas_price.load(Ordering::SeqCst)));
                    receipt["l1Fee"] = json!("0x7");
                    receipts.lock().unwrap().insert(hash, receipt);
                    json!(hash)
                },
                "eth_getTransactionReceipt" => {
                    let hash: B256 = serde_json::from_value(request["params"][0].clone()).unwrap();
                    if ready.load(Ordering::SeqCst) { receipts.lock().unwrap().get(&hash).cloned().unwrap_or(Value::Null) } else { Value::Null }
                },
                _ => panic!("unexpected RPC: {}", request["method"]),
            };
            std::future::ready(result)
        }).await;
        let oracle = PriceOracle::for_test(PriceSnapshot::from_prices([(
            c.native_symbol(),
            parse_usd_amount("2000").unwrap(),
        )]));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("progress.json");
        Self {
            config: c,
            wallet,
            client,
            rpc,
            oracle,
            dir,
            path,
            sends,
            receipt_ready,
            revert_all,
            reject_broadcast,
            ambiguous_broadcast,
            receipt_gas_price,
            broadcast_calls,
            servers: vec![server, opensea_server],
        }
    }
    async fn run(&self, dry_run: bool) -> Result<()> {
        let _keep_directory = &self.dir;
        tokio::time::timeout(
            Duration::from_secs(15),
            BuyRunner {
                config: &self.config,
                client: &self.client,
                rpc: &self.rpc,
                oracle: &self.oracle,
                wallet: &self.wallet,
                slug: "test",
                path: &self.path,
            }
            .run(dry_run),
        )
        .await
        .expect("purchase loop did not finish")
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        for server in &self.servers {
            server.abort();
        }
    }
}

#[tokio::test]
async fn buys_two_confirmed_nfts_on_every_chain_and_stops_without_rebuying_after_restart() {
    for chain in [4663, 57073, 999, 2741] {
        let mut f = Fixture::new(chain, false, false).await;
        if chain == 2741 {
            f.config.gas_mode = OpenSeaExecutionMode::Aggressive;
        }
        f.run(false).await.unwrap();
        assert_eq!(f.sends.load(Ordering::SeqCst), 2, "chain {chain}");
        assert_eq!(BuyProgress::load(&f.path).unwrap().purchased, 2);
        f.run(false).await.unwrap();
        assert_eq!(f.sends.load(Ordering::SeqCst), 2);
    }
}
#[tokio::test]
async fn a_reverted_purchase_does_not_count_and_loop_continues_to_two_successes() {
    let f = Fixture::new(4663, true, false).await;
    f.run(false).await.unwrap();
    assert_eq!(f.sends.load(Ordering::SeqCst), 3);
    assert_eq!(BuyProgress::load(&f.path).unwrap().purchased, 2);
}
#[tokio::test]
async fn pending_timeout_stops_new_buys_and_restart_reconciles_before_continuing() {
    let f = Fixture::new(4663, false, false).await;
    f.receipt_ready.store(false, Ordering::SeqCst);
    assert!(matches!(
        f.run(false).await,
        Err(BotError::BroadcastOutcomeUnknown { .. })
    ));
    assert_eq!(f.sends.load(Ordering::SeqCst), 1);
    assert!(BuyProgress::load(&f.path).unwrap().pending.is_some());
    f.receipt_ready.store(true, Ordering::SeqCst);
    f.run(false).await.unwrap();
    assert_eq!(f.sends.load(Ordering::SeqCst), 2);
    assert_eq!(BuyProgress::load(&f.path).unwrap().purchased, 2);
}
#[tokio::test]
async fn dry_run_and_repriced_fulfillment_never_broadcast_or_change_progress() {
    for wrong_price in [false, true] {
        let f = Fixture::new(4663, false, wrong_price).await;
        f.run(true).await.unwrap();
        assert_eq!(f.sends.load(Ordering::SeqCst), 0);
        assert!(!f.path.exists());
    }
}

#[test]
fn regular_and_partial_erc1155_advanced_orders_validate_the_encoded_asset_and_recipient() {
    const ORDER: &str = "(address,address,(uint8,address,uint256,uint256,uint256)[],(uint8,address,uint256,uint256,uint256,address)[],uint8,uint256,uint256,bytes32,uint256,bytes32,uint256)";
    let c = config(2741);
    let buyer = Address::repeat_byte(2);
    let mut listing = listing_json(&c, 1);
    let mut params = listing["protocol_data"]["parameters"].clone();
    let extra = json!({"zone":Address::ZERO, "orderType":0, "startTime":"1", "endTime":"9999999999",
        "zoneHash":B256::ZERO, "salt":"1", "conduitKey":B256::ZERO, "totalOriginalConsiderationItems":"1"});
    params
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    let mut f = fulfillment(&c, 1);
    f.transaction.function = format!("fulfillOrder(({ORDER},bytes),bytes32)");
    f.transaction.input_data = json!({"order":{"parameters":params, "signature":"0x01"}});
    let selected = parse_listing(&listing, &c, buyer).unwrap();
    assert!(validate_fulfillment(&f, &selected, &c, buyer, 100).is_ok());
    params["offer"][0]["itemType"] = json!(3);
    params["offer"][0]["startAmount"] = json!("2");
    params["offer"][0]["endAmount"] = json!("2");
    params["orderType"] = json!(1);
    params["consideration"][0]["startAmount"] = json!("50000000000000000");
    params["consideration"][0]["endAmount"] = json!("50000000000000000");
    listing["protocol_data"]["parameters"] = params.clone();
    listing["remaining_quantity"] = json!(2);
    let selected = parse_listing(&listing, &c, buyer).unwrap();
    assert_eq!(selected.value, parse_native_amount("0.025").unwrap());
    f.transaction.function = format!(
        "fulfillAdvancedOrder(({ORDER},uint120,uint120,bytes,bytes),(uint256,uint8,uint256,uint256,bytes32[])[],bytes32,address)"
    );
    f.transaction.input_data = json!({"advancedOrder":{"parameters":params,"numerator":1,"denominator":2,"signature":"0x01","extraData":"0x"},
        "criteriaResolvers":[], "recipient":Address::repeat_byte(9)});
    let encoded = validate_fulfillment(&f, &selected, &c, buyer, 100).unwrap();
    use alloy::dyn_abi::JsonAbiExt;
    let decoded = crate::abi::parse_function(&f.transaction.function)
        .unwrap()
        .abi_decode_input(&encoded[4..])
        .unwrap();
    assert_eq!(
        decoded.last(),
        Some(&alloy::dyn_abi::DynSolValue::Address(buyer))
    );
    f.transaction.input_data["advancedOrder"]["numerator"] = json!(2);
    assert!(validate_fulfillment(&f, &selected, &c, buyer, 100).is_err());
}

#[tokio::test]
async fn aggressive_gas_raises_fee_bid_and_both_modes_enforce_the_gas_budget() {
    let mut f = Fixture::new(2741, false, false).await;
    let (_, normal) = find_purchase(
        &f.config,
        &f.client,
        &f.rpc,
        &f.oracle,
        "test",
        f.wallet.address,
        &mut BuyProgress::default(),
    )
    .await
    .unwrap()
    .unwrap();
    f.config.gas_mode = OpenSeaExecutionMode::Aggressive;
    let (_, aggressive) = find_purchase(
        &f.config,
        &f.client,
        &f.rpc,
        &f.oracle,
        "test",
        f.wallet.address,
        &mut BuyProgress::default(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(aggressive.max_fee_per_gas > normal.max_fee_per_gas);
    assert!(aggressive.max_priority_fee_per_gas > normal.max_priority_fee_per_gas);
    assert_eq!(aggressive.gas, normal.gas);
    for mode in [
        OpenSeaExecutionMode::Normal,
        OpenSeaExecutionMode::Aggressive,
    ] {
        f.config.gas_mode = mode;
        f.config.max_gas_cost_native = "0.000000000000000001".into();
        assert!(
            find_purchase(
                &f.config,
                &f.client,
                &f.rpc,
                &f.oracle,
                "test",
                f.wallet.address,
                &mut BuyProgress::default()
            )
            .await
            .unwrap()
            .is_none()
        );
    }
    assert_eq!(f.sends.load(Ordering::SeqCst), 0);
}

async fn stage_unsent_buy(f: &Fixture, attempted: bool) -> PendingBuy {
    let (listing, request) = find_purchase(
        &f.config,
        &f.client,
        &f.rpc,
        &f.oracle,
        "test",
        f.wallet.address,
        &mut BuyProgress::default(),
    )
    .await
    .unwrap()
    .unwrap();
    let nonce = request.nonce;
    let raw = f.wallet.sign_request(request).await.unwrap().encoded_2718();
    let pending = PendingBuy {
        hash: keccak256(&raw),
        order_hash: listing.hash,
        token_id: listing.token_id,
        item_type: listing.item_type,
        nonce,
        raw_transaction: Some(raw.into()),
        broadcast_attempted: attempted,
    };
    BuyProgress {
        pending: Some(pending.clone()),
        ..Default::default()
    }
    .save(&f.path)
    .unwrap();
    pending
}

#[tokio::test]
async fn failed_orders_are_not_retried_even_after_restart() {
    let f = Fixture::new(4663, false, false).await;
    f.revert_all.store(true, Ordering::SeqCst);
    assert!(
        tokio::time::timeout(Duration::from_millis(3300), f.run(false))
            .await
            .is_err()
    );
    let progress = BuyProgress::load(&f.path).unwrap();
    assert_eq!(progress.purchased, 0);
    assert_eq!(progress.failed_orders.len(), 3);
    assert_eq!(f.sends.load(Ordering::SeqCst), 3);
    f.revert_all.store(false, Ordering::SeqCst);
    f.run(true).await.unwrap();
    assert_eq!(f.broadcast_calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn gas_loss_budget_stops_before_another_attempt_and_survives_restart() {
    let mut f = Fixture::new(4663, false, false).await;
    f.revert_all.store(true, Ordering::SeqCst);
    f.receipt_gas_price.store(300, Ordering::SeqCst);
    f.config.max_failed_gas_cost_native = "0.0000000001".into();
    for _ in 0..2 {
        let error = f.run(false).await.unwrap_err();
        assert!(error.to_string().contains("gas-loss budget"), "{error}");
        assert_eq!(f.sends.load(Ordering::SeqCst), 2);
        let p = BuyProgress::load(&f.path).unwrap();
        assert_eq!(p.purchased, 0);
        assert_eq!(p.failed_gas_cost_native, U256::from(90_000_000));
        assert!(p.pending.is_none());
    }
}

#[tokio::test]
async fn ink_failed_gas_accounting_includes_l1_and_operator_fees() {
    let f = Fixture::new(57073, true, false).await;
    f.run(false).await.unwrap();
    assert_eq!(
        BuyProgress::load(&f.path).unwrap().failed_gas_cost_native,
        U256::from(150_008)
    );
}

#[tokio::test]
async fn crash_before_or_during_broadcast_recovers_identical_transaction_once() {
    for attempted in [false, true] {
        let mut f = Fixture::new(4663, false, false).await;
        f.config.quantity = 1;
        let pending = stage_unsent_buy(&f, attempted).await;
        assert_eq!(f.sends.load(Ordering::SeqCst), 0);
        f.run(false).await.unwrap();
        assert_eq!(f.sends.load(Ordering::SeqCst), 1);
        assert_eq!(f.broadcast_calls.load(Ordering::SeqCst), 1);
        assert!(
            f.rpc
                .transaction_receipt(pending.hash)
                .await
                .unwrap()
                .unwrap()
                .status()
        );
        f.run(false).await.unwrap();
        assert_eq!(f.broadcast_calls.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn already_mined_purchase_is_reconciled_without_rebroadcast() {
    let mut f = Fixture::new(4663, false, false).await;
    f.config.quantity = 1;
    let pending = stage_unsent_buy(&f, true).await;
    f.rpc
        .broadcast_raw(pending.raw_transaction.unwrap().to_vec())
        .await
        .unwrap();
    f.run(false).await.unwrap();
    assert_eq!(f.broadcast_calls.load(Ordering::SeqCst), 1);
    assert_eq!(BuyProgress::load(&f.path).unwrap().purchased, 1);
}

#[tokio::test]
async fn definitive_first_rejection_clears_pending_but_recovery_rejection_does_not() {
    for attempted in [false, true] {
        let f = Fixture::new(4663, false, false).await;
        let pending = stage_unsent_buy(&f, attempted).await;
        f.reject_broadcast.store(true, Ordering::SeqCst);
        let mut progress = BuyProgress::load(&f.path).unwrap();
        let result = BuyRunner {
            config: &f.config,
            client: &f.client,
            rpc: &f.rpc,
            oracle: &f.oracle,
            wallet: &f.wallet,
            slug: "test",
            path: &f.path,
        }
        .recover_pending(&mut progress)
        .await;
        let stored = BuyProgress::load(&f.path).unwrap();
        assert_eq!(stored.purchased, 0);
        assert_eq!(stored.failed_gas_cost_native, U256::ZERO);
        if attempted {
            assert!(matches!(
                result,
                Err(BotError::BroadcastOutcomeUnknown { .. })
            ));
            assert!(stored.pending.is_some());
            assert!(stored.failed_orders.is_empty());
        } else {
            result.unwrap();
            assert!(stored.pending.is_none());
            assert!(stored.failed_orders.contains(&pending.order_hash));
        }
    }
}

#[tokio::test]
async fn ambiguous_broadcast_preserves_bytes_and_resumes_without_new_nonce() {
    let mut f = Fixture::new(4663, false, false).await;
    f.config.quantity = 1;
    let pending = stage_unsent_buy(&f, false).await;
    f.ambiguous_broadcast.store(true, Ordering::SeqCst);
    assert!(matches!(
        f.run(false).await,
        Err(BotError::BroadcastOutcomeUnknown { .. })
    ));
    let saved = BuyProgress::load(&f.path).unwrap().pending.unwrap();
    assert_eq!(saved.raw_transaction, pending.raw_transaction);
    assert_eq!(saved.nonce, pending.nonce);
    assert!(saved.broadcast_attempted);
    f.ambiguous_broadcast.store(false, Ordering::SeqCst);
    f.run(false).await.unwrap();
    assert_eq!(f.broadcast_calls.load(Ordering::SeqCst), 2);
    assert_eq!(f.sends.load(Ordering::SeqCst), 1);
    assert!(
        f.rpc
            .transaction_receipt(pending.hash)
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn recovery_rejects_corrupt_saved_data_and_handles_legacy_journals_without_guessing() {
    let f = Fixture::new(4663, false, false).await;
    let pending = stage_unsent_buy(&f, false).await;
    for change in 0..4 {
        let mut corrupted = pending.clone();
        match change {
            0 => corrupted.hash = B256::ZERO,
            1 => corrupted.nonce = Some(99),
            2 => corrupted.raw_transaction = Some(Bytes::from(vec![0])),
            _ => {}
        }
        let buyer = if change == 3 {
            Address::ZERO
        } else {
            f.wallet.address
        };
        assert!(validate_saved_transaction(&corrupted, f.config.chain_id, buyer).is_err());
    }
    let legacy = json!({"purchased":0,"completed_orders":[],"purchased_erc721":[],
        "pending":{"hash":pending.hash,"order_hash":pending.order_hash,"token_id":pending.token_id,"item_type":2}});
    std::fs::write(&f.path, serde_json::to_vec(&legacy).unwrap()).unwrap();
    let p = BuyProgress::load(&f.path).unwrap();
    assert_eq!(p.failed_gas_cost_native, U256::ZERO);
    assert!(p.pending.as_ref().unwrap().broadcast_attempted);
    assert!(matches!(
        f.run(false).await,
        Err(BotError::BroadcastOutcomeUnknown { .. })
    ));
    assert_eq!(f.broadcast_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn pagination_checks_floor_each_cycle_and_keeps_advancing_later_pages() {
    let mut f = Fixture::new(2741, false, false).await;
    f.config.poll_seconds = 5;
    let routes = Arc::new(Mutex::new(Vec::<String>::new()));
    let observed = routes.clone();
    let floor_ready = Arc::new(AtomicBool::new(false));
    let ready = floor_ready.clone();
    let c = f.config.clone();
    let (client, server) = mock_opensea_responder(move |route, request| {
        if route.starts_with("POST") {
            return opensea_response(&c, false, route, request);
        }
        observed.lock().unwrap().push(route.to_owned());
        let uri = route.split_whitespace().nth(1).unwrap();
        let url = reqwest::Url::parse(&format!("http://localhost{uri}")).unwrap();
        let next = url
            .query_pairs()
            .find(|(key, _)| key == "next")
            .map(|(_, value)| value.parse::<usize>().unwrap());
        if next.is_none() && ready.load(Ordering::SeqCst) {
            return json!({"listings":[listing_json(&c, 1)], "next":"1"});
        }
        json!({"listings":[],"next":(next.unwrap_or(0) + 1).to_string()})
    })
    .await;
    f.client = client;
    f.servers.push(server);
    let mut progress = BuyProgress::default();
    for _ in 0..3 {
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            find_purchase(
                &f.config,
                &f.client,
                &f.rpc,
                &f.oracle,
                "test",
                f.wallet.address,
                &mut progress,
            ),
        )
        .await
        .expect("discovery must not wait five seconds per page")
        .unwrap();
        assert!(result.is_none());
    }
    let calls = routes.lock().unwrap().clone();
    assert_eq!(calls.len(), 6);
    for i in 0..3 {
        assert!(!calls[i * 2].contains("next="));
        assert!(calls[i * 2 + 1].contains(&format!("next={}", i + 1)));
    }
    floor_ready.store(true, Ordering::SeqCst);
    let result = find_purchase(
        &f.config,
        &f.client,
        &f.rpc,
        &f.oracle,
        "test",
        f.wallet.address,
        &mut progress,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(result.0.token_id, U256::from(1));
    assert_eq!(routes.lock().unwrap().len(), 7);
}
