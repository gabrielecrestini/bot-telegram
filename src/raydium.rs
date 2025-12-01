use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    transaction::Transaction,
    system_instruction,
    commitment_config::CommitmentConfig,
    compute_budget::ComputeBudgetInstruction, 
};
use solana_client::{
    rpc_config::{RpcProgramAccountsConfig, RpcAccountInfoConfig},
    rpc_filter::{RpcFilterType, Memcmp, MemcmpEncodedBytes},
};
use solana_account_decoder::UiAccountEncoding;
use borsh::{BorshSerialize, BorshDeserialize};
use std::sync::Arc;
use std::str::FromStr;
use crate::network::NetworkClient;
use log::{info, warn};

// Program ID Ufficiali
pub const RAYDIUM_V4_PROGRAM_ID: &str = "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8";
pub const SERUM_PROGRAM_ID: &str = "srmqPvymJeFKQ4zGQed1GFppgkRHL9kaELCbyksJtPX"; 

// Struttura Dati Istruzione Swap (Borsh)
#[derive(BorshSerialize, BorshDeserialize, Debug)]
pub struct SwapInstructionData {
    pub instruction: u8, 
    pub amount_in: u64,
    pub min_amount_out: u64,
}

// Struttura Chiavi Pool (Tutto ciò che serve per interagire con l'AMM)
#[derive(Debug, Clone)]
pub struct RaydiumPoolKeys {
    pub amm_id: Pubkey,
    pub amm_authority: Pubkey,
    pub amm_open_orders: Pubkey,
    pub amm_target_orders: Pubkey,
    pub amm_coin_vault: Pubkey,
    pub amm_pc_vault: Pubkey,
    pub market_program_id: Pubkey,
    pub market_id: Pubkey,
    pub market_bids: Pubkey,
    pub market_asks: Pubkey,
    pub market_event_queue: Pubkey,
    pub market_coin_vault: Pubkey,
    pub market_pc_vault: Pubkey,
    pub market_vault_signer: Pubkey,
}

// Struttura Dati on-chain AMM (Layout di memoria)
#[derive(BorshDeserialize, Debug)]
pub struct AmmInfo {
    pub status: u64,
    pub nonce: u64,
    pub order_num: u64,
    pub depth: u64,
    pub coin_decimals: u64,
    pub pc_decimals: u64,
    pub state: u64,
    pub reset_flag: u64,
    pub min_size: u64,
    pub vol_max_cut_ratio: u64,
    pub amount_wave: u64,
    pub coin_lot_size: u64,
    pub pc_lot_size: u64,
    pub min_price_multiplier: u64,
    pub max_price_multiplier: u64,
    pub system_decimal_value: u64,
    pub min_separate_numerator: u64,
    pub min_separate_denominator: u64,
    pub trade_fee_numerator: u64,
    pub trade_fee_denominator: u64,
    pub pnl_numerator: u64,
    pub pnl_denominator: u64,
    pub swap_fee_numerator: u64,
    pub swap_fee_denominator: u64,
    pub need_take_pnl_coin: u64,
    pub need_take_pnl_pc: u64,
    pub total_pnl_pc: u64,
    pub total_pnl_coin: u64,
    pub pool_total_deposit_pc: u128,
    pub pool_total_deposit_coin: u128,
    pub swap_coin_in_amount: u128,
    pub swap_pc_out_amount: u128,
    pub swap_coin_2pc_fee: u64,
    pub swap_pc_in_amount: u128,
    pub swap_coin_out_amount: u128,
    pub swap_pc_2coin_fee: u64,
    pub pool_coin_token_account: Pubkey,
    pub pool_pc_token_account: Pubkey,
    pub coin_mint_address: Pubkey,
    pub pc_mint_address: Pubkey,
    pub lp_mint_address: Pubkey,
    pub amm_open_orders: Pubkey,
    pub amm_target_orders: Pubkey,
    pub pool_withdraw_queue: Pubkey,
    pub pool_temp_lp_token_account: Pubkey,
    pub amm_owner: Pubkey,
    pub pnl_owner: Pubkey,
}

/// Trova la Pool Raydium partendo dal Mint del Token
pub async fn fetch_pool_keys_by_mint(
    network: &Arc<NetworkClient>, 
    token_mint: &Pubkey,
) -> Result<RaydiumPoolKeys, Box<dyn std::error::Error + Send + Sync>> {
    
    // info!("🔎 Cerco Liquidity Pool per il token: {}", token_mint);
    let raydium_prog = Pubkey::from_str(RAYDIUM_V4_PROGRAM_ID)?;
    let wsol_mint = spl_token::native_mint::id();

    let filters = vec![
        RpcFilterType::Memcmp(Memcmp::new(400, MemcmpEncodedBytes::Base58(token_mint.to_string()))),
        RpcFilterType::Memcmp(Memcmp::new(432, MemcmpEncodedBytes::Base58(wsol_mint.to_string()))),
    ];

    let accounts = network.rpc.get_program_accounts_with_config(
        &raydium_prog,
        RpcProgramAccountsConfig {
            filters: Some(filters),
            account_config: RpcAccountInfoConfig {
                encoding: Some(UiAccountEncoding::Base64),
                data_slice: None,
                commitment: Some(CommitmentConfig::confirmed()),
                min_context_slot: None,
            },
            with_context: Some(true),
        },
    ).await?;

    if accounts.is_empty() {
        return Err("Pool Raydium non trovata (Verifica che sia una coppia SOL).".into());
    }

    let (amm_id, account) = &accounts[0];
    let data = &account.data;
    
    let amm_info: AmmInfo = BorshDeserialize::try_from_slice(&data[..data.len().min(752)])
        .map_err(|_| "Errore parsing dati AMM Raydium")?;

    let market_id_bytes: [u8; 32] = data[504..536].try_into()?;
    let market_id = Pubkey::new_from_array(market_id_bytes);

    // Fetch OpenBook Market
    let market_account = network.rpc.get_account(&market_id).await?;
    let market_data = market_account.data;

    // Offset fissi per Serum V3 Market Layout
    let market_bids = Pubkey::new_from_array(market_data[285..317].try_into()?);
    let market_asks = Pubkey::new_from_array(market_data[317..349].try_into()?);
    let market_event_queue = Pubkey::new_from_array(market_data[349..381].try_into()?);
    let market_coin_vault = Pubkey::new_from_array(market_data[125..157].try_into()?);
    let market_pc_vault = Pubkey::new_from_array(market_data[157..189].try_into()?);
    
    let vault_signer_nonce = u64::from_le_bytes(market_data[45..53].try_into()?);
    let market_vault_signer = Pubkey::create_program_address(
        &[&market_id.to_bytes(), &vault_signer_nonce.to_le_bytes()],
        &Pubkey::from_str(SERUM_PROGRAM_ID)?,
    )?;

    let amm_authority = Pubkey::from_str("5Q544fKrFoe6tsEbD7S8EmxGTJYAKtTVhAW5Q5pge4j1")?;

    Ok(RaydiumPoolKeys {
        amm_id: amm_id.clone(), amm_authority, amm_open_orders: amm_info.amm_open_orders, amm_target_orders: amm_info.amm_target_orders, amm_coin_vault: amm_info.pool_coin_token_account, amm_pc_vault: amm_info.pool_pc_token_account, market_program_id: Pubkey::from_str(SERUM_PROGRAM_ID)?, market_id, market_bids, market_asks, market_event_queue, market_coin_vault, market_pc_vault, market_vault_signer,
    })
}

/// Esegue lo Swap su Raydium V4
pub async fn execute_swap(
    network: &Arc<NetworkClient>,
    payer: &Keypair,
    pool_keys: &RaydiumPoolKeys,
    token_mint_address: Pubkey, 
    amount_in: u64, 
    slippage_bps: u64 
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {

    let user = payer.pubkey();
    let wsol_mint = spl_token::native_mint::id();
    let program_id = Pubkey::from_str(RAYDIUM_V4_PROGRAM_ID)?;

    // CALCOLO MINIMO OUT
    // Per lo sniping veloce su Raydium diretto, impostiamo min_out a 0 per evitare fallimenti dovuti a volatilità estrema.
    // La protezione reale viene fatta a monte dall'analisi di sicurezza e liquidità.
    // Se stiamo usando Jupiter (in jupiter.rs), lì lo slippage è gestito precisamente.
    // Qui, se siamo su Raydium diretto, stiamo cercando velocità pura.
    let min_amount_out = 0; 

    let mut instructions = Vec::new();

    // 1. PRIORITY FEES OTTIMIZZATE
    // Swap AMM = ~150,000-200,000 CU
    // 100,000 µLamp/CU × 200,000 CU = 20,000 lamports = 0.00002 SOL (~$0.004)
    // Abbastanza per battere congestione senza bruciare capitale
    instructions.push(ComputeBudgetInstruction::set_compute_unit_price(100_000));  // Priorità alta ma ragionevole
    instructions.push(ComputeBudgetInstruction::set_compute_unit_limit(200_000));  // Swap complesso

    // 2. GESTIONE WSOL (Wrap SOL)
    let wsol_ata = spl_associated_token_account::get_associated_token_address(&user, &wsol_mint);
    instructions.push(spl_associated_token_account::instruction::create_associated_token_account_idempotent(&user, &user, &wsol_mint, &spl_token::id()));
    instructions.push(system_instruction::transfer(&user, &wsol_ata, amount_in));
    instructions.push(spl_token::instruction::sync_native(&spl_token::id(), &wsol_ata)?);

    // 3. GESTIONE TOKEN DESTINAZIONE (Create ATA)
    let token_ata = spl_associated_token_account::get_associated_token_address(&user, &token_mint_address);
    instructions.push(spl_associated_token_account::instruction::create_associated_token_account_idempotent(&user, &user, &token_mint_address, &spl_token::id()));

    // 4. SWAP INSTRUCTION
    let data = SwapInstructionData {
        instruction: 9, // swapBaseIn
        amount_in,
        min_amount_out, 
    };
    
    let accounts = vec![
        AccountMeta::new_readonly(spl_token::id(), false),
        AccountMeta::new(pool_keys.amm_id, false),
        AccountMeta::new_readonly(pool_keys.amm_authority, false),
        AccountMeta::new(pool_keys.amm_open_orders, false),
        AccountMeta::new(pool_keys.amm_target_orders, false),
        AccountMeta::new(pool_keys.amm_coin_vault, false),
        AccountMeta::new(pool_keys.amm_pc_vault, false),
        AccountMeta::new_readonly(pool_keys.market_program_id, false),
        AccountMeta::new(pool_keys.market_id, false),
        AccountMeta::new(pool_keys.market_bids, false),
        AccountMeta::new(pool_keys.market_asks, false),
        AccountMeta::new(pool_keys.market_event_queue, false),
        AccountMeta::new(pool_keys.market_coin_vault, false),
        AccountMeta::new(pool_keys.market_pc_vault, false),
        AccountMeta::new_readonly(pool_keys.market_vault_signer, false),
        AccountMeta::new(wsol_ata, false), 
        AccountMeta::new(token_ata, false),
        AccountMeta::new_readonly(user, true),
    ];

    instructions.push(Instruction { program_id, accounts, data: data.try_to_vec()? });

    // 5. CLOSE WSOL (Recupero Rent)
    instructions.push(spl_token::instruction::close_account(&spl_token::id(), &wsol_ata, &user, &user, &[])?);

    // 6. FIRMA E INVIO
    let recent_blockhash = network.rpc.get_latest_blockhash().await?;
    let transaction = Transaction::new_signed_with_payer(&instructions, Some(&user), &[payer], recent_blockhash);
    let signature = transaction.signatures[0];

    // Invio via TPU (QUIC) per saltare la coda
    network.tpu.send_transaction(&transaction);
    
    // Ritorniamo subito la firma per monitoraggio, non aspettiamo la conferma qui (asincrono)
    Ok(signature.to_string())
}