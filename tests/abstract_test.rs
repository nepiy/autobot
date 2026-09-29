use assert_cmd::Command;
use predicates::str::contains;

fn isolated_bot(directory: &std::path::Path) -> Command {
    let mut command = Command::new(assert_cmd::cargo::cargo_bin!("nft-mint-bot"));
    command.current_dir(directory).env_clear();
    command
}

#[test]
fn abstract_normal_launcher_selects_chain_and_validates_config() {
    let directory = tempfile::tempdir().unwrap();
    isolated_bot(directory.path())
        .args(["start", "--dry-run"])
        .write_stdin(
            "2\n4\n0x0000000000000000000000000000000000000001\nabstract-drop\n1\nyes\nnormal\nno\n",
        )
        .assert()
        .failure()
        .stdout(contains("Network: Abstract mainnet (chain ID 2741)"))
        .stdout(contains("Execution mode: normal"))
        .stderr(contains("PRIVATE_KEY is not set"));
}

#[test]
fn abstract_aggressive_launcher_requires_a_tested_gas_limit() {
    let directory = tempfile::tempdir().unwrap();
    isolated_bot(directory.path())
        .args(["start", "--dry-run"])
        .write_stdin(
            "2\n4\n0x0000000000000000000000000000000000000001\nabstract-drop\n1\nyes\naggressive\n0\n",
        )
        .assert()
        .failure()
        .stderr(contains("Abstract gas limit must be greater than zero"));

    isolated_bot(directory.path())
        .args(["start", "--dry-run"])
        .write_stdin(
            "2\n4\n0x0000000000000000000000000000000000000001\nabstract-drop\n1\nyes\naggressive\n1500000\nno\n",
        )
        .assert()
        .failure()
        .stdout(contains("Network: Abstract mainnet (chain ID 2741)"))
        .stderr(contains("PRIVATE_KEY is not set"));
}
