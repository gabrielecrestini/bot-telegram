use solana_sdk::{
    pubkey::Pubkey,
    program_pack::Pack, 
};
use spl_token::state::Mint; 
use std::sync::Arc;
use crate::network::NetworkClient;

pub struct TokenSafetyReport {
    pub is_safe: bool,
    pub mint_authority_disabled: bool,
    pub freeze_authority_disabled: bool,
    pub supply: u64,
    pub decimals: u8,
    pub reason: String,
}

/// Analizza un token per vedere se è una potenziale truffa (Rug/Honeypot)
pub async fn check_token_safety(
    network: &Arc<NetworkClient>,
    token_mint: &Pubkey
) -> Result<TokenSafetyReport, Box<dyn std::error::Error + Send + Sync>> {

    // 1. Scarica i dati dell'account del Token (Mint Account)
    let account = network.rpc.get_account(token_mint).await?;

    // 2. Decodifica i dati grezzi nella struttura Mint di SPL Token
    let mint_data = Mint::unpack(&account.data)
        .map_err(|_| "Impossibile decodificare i dati del Token")?;

    // 3. ANALISI ANTI-RUG (Mint Authority)
    // Se è None, la supply è fissa (SAFE).
    let mint_auth_disabled = mint_data.mint_authority.is_none();

    // 4. ANALISI ANTI-HONEYPOT (Freeze Authority)
    // Se è None, lo sviluppatore NON può bloccare i trasferimenti.
    let freeze_auth_disabled = mint_data.freeze_authority.is_none();

    // 5. Verdetto Finale
    let mut is_safe = true;
    let mut reasons = Vec::new();

    if !mint_auth_disabled {
        is_safe = false;
        reasons.push("⚠️ Mint Auth Attiva");
    }

    if !freeze_auth_disabled {
        is_safe = false; 
        reasons.push("❄️ Freeze Auth Attiva");
    }

    let report_string = if is_safe {
        "✅ Token Sicuro".to_string()
    } else {
        reasons.join(" | ")
    };

    Ok(TokenSafetyReport {
        is_safe,
        mint_authority_disabled: mint_auth_disabled,
        freeze_authority_disabled: freeze_auth_disabled,
        supply: mint_data.supply,
        decimals: mint_data.decimals,
        reason: report_string,
    })
}