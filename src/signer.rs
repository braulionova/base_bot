use alloy::primitives::Address;
use alloy::providers::ProviderBuilder;
use alloy::signers::local::PrivateKeySigner;
use eyre::Result;
use tracing::info;

/// Create a provider with signing capabilities from a private key hex string.
/// This provider can send signed transactions directly.
pub async fn create_signing_provider(
    rpc_url: &str,
    private_key_hex: &str,
) -> Result<(impl alloy::providers::Provider + Clone, Address)> {
    // Parse private key (strip 0x prefix if present)
    let key_hex = private_key_hex.strip_prefix("0x").unwrap_or(private_key_hex);
    let signer: PrivateKeySigner = key_hex.parse()?;
    let wallet_address = signer.address();

    info!("Signing wallet: {}", wallet_address);

    // Build provider with wallet signer
    let provider = ProviderBuilder::new()
        .wallet(alloy::network::EthereumWallet::from(signer))
        .connect_http(rpc_url.parse()?);

    Ok((provider, wallet_address))
}
