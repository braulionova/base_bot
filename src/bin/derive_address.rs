use alloy::signers::local::PrivateKeySigner;

fn main() {
    let key = std::env::var("PRIVATE_KEY").expect("Set PRIVATE_KEY env var");
    let key_hex = key.strip_prefix("0x").unwrap_or(&key);
    let signer: PrivateKeySigner = key_hex.parse().expect("Invalid private key");
    println!("{}", signer.address());
}
