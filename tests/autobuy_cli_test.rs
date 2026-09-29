use assert_cmd::Command;
use predicates::str::contains;

#[test]
fn launcher_routes_auto_buy_for_all_four_chains() {
    for (choice, name, symbol) in [
        (1, "robinhood", "ETH"),
        (2, "ink", "ETH"),
        (3, "hyperevm", "HYPE"),
        (4, "abstract", "ETH"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        Command::new(assert_cmd::cargo::cargo_bin!("nft-mint-bot"))
            .current_dir(dir.path()).env_clear().args(["start", "--dry-run"])
            .write_stdin(format!("1\n{choice}\n0x0000000000000000000000000000000000000001\n50\n10\n2\naggressive\n0.001\ntest\n"))
            .assert().failure()
            .stdout(contains("1. Auto-buy\n2. Mint and auto-sell"))
            .stdout(contains(format!("Auto-buy: {name}")))
            .stdout(contains("$45.000000–$55.000000"))
            .stdout(contains(format!("Purchases use native {symbol}")))
            .stderr(contains("PRIVATE_KEY is not set"));
    }
}

#[test]
fn auto_buy_configuration_and_direct_subcommand_validate_before_wallet_loading() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("buy.json");
    std::fs::write(&path, include_str!("../configs/auto-buy.example.json")).unwrap();
    Command::new(assert_cmd::cargo::cargo_bin!("nft-mint-bot"))
        .current_dir(dir.path())
        .env_clear()
        .args(["auto-buy", "--config"])
        .arg(&path)
        .arg("--dry-run")
        .assert()
        .failure()
        .stdout(contains("Auto-buy: robinhood"))
        .stderr(contains("PRIVATE_KEY is not set"));
    Command::new(assert_cmd::cargo::cargo_bin!("nft-mint-bot"))
        .current_dir(dir.path())
        .env_clear()
        .args(["auto-buy", "--dry-run"])
        .write_stdin(
            "1\n0x0000000000000000000000000000000000000001\n50\n101\n2\nnormal\n0.001\ntest\n",
        )
        .assert()
        .failure()
        .stderr(contains("price tolerance must be 0–100%"));
}
