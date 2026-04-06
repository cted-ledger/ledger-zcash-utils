use assert_cmd::Command;
use std::time::Duration;

const KNOWN_MNEMONIC: &str =
    "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

/// UFVK derived from [`KNOWN_MNEMONIC`] on mainnet (account 0).
/// Used by sync tests that must reach the network layer before failing.
const KNOWN_UFVK_MAINNET: &str = "uview1qggz6nejagvka9wtm9r7xf84kkwy4cc0cgchptr98w0cyz33cj4958q5ulkd32nz2u3s0sp9yhcw7tu2n3nlw9x6ulghyd2zgc857tnzme2zpr3vn24zhtm2rjduv9a5zxlmzz404n7l0k69gmu4tfn2g3vpcn03rhz63e3l92fn8gra37tyly7utvgveswl20vz23pu84rc2nyqess38wvlgr2xzyhgj232ne5qutpe6ql6ghzetdy7pfzcmdzd5gd5dnwk25fwv7nnzmnty7u5ax3nzzgr6pdc905ckpd0s9v2cvn7e03qm7r46e5ngax536ywz7zxjptymm90px0rhvmqtwvttuy6d7degly023lqvskclk6mezyt69dwu6c4tfzrjgq4uuh5xa9m5dclgatykgtrrw268qe5pldfkx73f2kd5yyy2tjpjql92pa6tsk2nh2h88q23nee9z379het4akl6haqmuwf9d0nl0susg4tnxyk";

/// A loopback URL guaranteed to give ECONNREFUSED: port 1 is a privileged
/// port that no user-space service ever listens on.
fn refused_grpc_url() -> &'static str {
    "https://127.0.0.1:1"
}

#[test]
fn test_derive_json_output_has_ufvk_key() {
    let mut cmd = Command::cargo_bin("ledger-zcash-cli").unwrap();
    cmd.args(["derive", "--mnemonic", KNOWN_MNEMONIC, "--format", "json"])
        .assert()
        .success()
        .stdout(predicates::str::contains("\"ufvk\""));
}

#[test]
fn test_derive_json_mainnet_ufvk_prefix() {
    let mut cmd = Command::cargo_bin("ledger-zcash-cli").unwrap();
    cmd.args([
        "derive",
        "--mnemonic",
        KNOWN_MNEMONIC,
        "--network",
        "mainnet",
        "--format",
        "json",
    ])
    .assert()
    .success()
    .stdout(predicates::str::contains("uview1"));
}

#[test]
fn test_derive_json_testnet_ufvk_prefix() {
    let mut cmd = Command::cargo_bin("ledger-zcash-cli").unwrap();
    cmd.args([
        "derive",
        "--mnemonic",
        KNOWN_MNEMONIC,
        "--network",
        "testnet",
        "--format",
        "json",
    ])
    .assert()
    .success()
    .stdout(predicates::str::contains("uviewtest1"));
}

#[test]
fn test_derive_human_output_has_ufvk_label() {
    let mut cmd = Command::cargo_bin("ledger-zcash-cli").unwrap();
    cmd.args(["derive", "--mnemonic", KNOWN_MNEMONIC, "--format", "human"])
        .assert()
        .success()
        .stdout(predicates::str::contains("ufvk"));
}

#[test]
fn test_derive_invalid_mnemonic_exits_nonzero() {
    let mut cmd = Command::cargo_bin("ledger-zcash-cli").unwrap();
    cmd.args(["derive", "--mnemonic", "not a valid mnemonic phrase here"])
        .assert()
        .failure();
}

// ─── derive tests ─────────────────────────────────────────────────────────────

#[test]
fn test_derive_no_sapling_flag() {
    let mut cmd = Command::cargo_bin("ledger-zcash-cli").unwrap();
    cmd.args([
        "derive",
        "--mnemonic",
        KNOWN_MNEMONIC,
        "--no-sapling",
        "--format",
        "json",
    ])
    .assert()
    .success()
    .stdout(predicates::str::contains("\"ufvk\""));
}

// ─── sync network error tests ─────────────────────────────────────────────────

/// A malformed URL is detected before any network call: the command must exit
/// with a non-zero code and print an error to stderr.
#[test]
fn sync_exits_nonzero_on_malformed_url() {
    Command::cargo_bin("ledger-zcash-cli")
        .unwrap()
        .timeout(Duration::from_secs(15))
        .args([
            "sync",
            "--grpc-url", "not_a_valid_url !!!",
            "--viewing-key", KNOWN_UFVK_MAINNET,
            "--start-height", "2000000",
            "--end-height", "2000010",
            "--network", "mainnet",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("Error"));
}

/// When the TCP port is closed the command must fail quickly (ECONNREFUSED)
/// with a clear error message, not block indefinitely.
#[test]
fn sync_exits_nonzero_on_connection_refused() {
    let url = refused_grpc_url();

    Command::cargo_bin("ledger-zcash-cli")
        .unwrap()
        .timeout(Duration::from_secs(15))
        .args([
            "sync",
            "--grpc-url", &url,
            "--viewing-key", KNOWN_UFVK_MAINNET,
            "--start-height", "2000000",
            "--end-height", "2000010",
            "--network", "mainnet",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("Error"));
}
