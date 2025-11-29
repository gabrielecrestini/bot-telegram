use teloxide::{
    prelude::*,
    types::{InlineKeyboardButton, InlineKeyboardMarkup, WebAppInfo, ParseMode},
    utils::command::BotCommands,
};
use sqlx::SqlitePool;
use std::sync::Arc;
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;
use solana_sdk::native_token::LAMPORTS_PER_SOL;
use crate::network::NetworkClient;

// ⚠️ IMPORTANTE: SOSTITUISCI QUESTO CON IL TUO LINK NETLIFY
// Esempio: "https://tuo-sito-fantastico.netlify.app"
const WEB_APP_URL: &str = "https://cryptostarstudiobot.netlify.app"; 

// Stato Condiviso
pub struct BotState {
    pub pool: SqlitePool,
    pub network: Arc<NetworkClient>,
}

// Comandi Base
#[derive(BotCommands, Clone)]
#[command(rename_rule = "lowercase", description = "Comandi Disponibili:")]
enum Command {
    #[command(description = "Avvia il Pannello di Controllo")]
    Start,
    #[command(description = "Compra manuale: /buy INDIRIZZO IMPORTO")]
    Buy(String),
}

// --- 1. TASTIERA IBRIDA (WEB APP + AZIONI RAPIDE) ---
fn make_main_keyboard() -> InlineKeyboardMarkup {
    // Il Tasto Web App deve essere il protagonista
    let btn_app = InlineKeyboardButton::web_app(
        "🚀 APRI DASHBOARD DI TRADING", 
        WebAppInfo { url: WEB_APP_URL.parse().unwrap() }
    );
    
    InlineKeyboardMarkup::new(vec![
        // Riga 1: Web App (Gigante)
        vec![btn_app],
        
        // Riga 2: Controlli Bot Automatico
        vec![
            InlineKeyboardButton::callback("🤖 AVVIA AUTO-BOT (24h)", "start_auto_bot"),
            InlineKeyboardButton::callback("✋ STOP BOT", "stop_auto_bot"),
        ],
        
        // Riga 3: Gestione Fondi
        vec![
            InlineKeyboardButton::callback("💰 Saldo Wallet", "balance"),
            InlineKeyboardButton::callback("💸 PRELEVA FONDI", "withdraw_all"),
        ],
        
        // Riga 4: Refresh Rapido
        vec![
            InlineKeyboardButton::callback("🔄 Aggiorna Stato", "refresh_home"),
        ]
    ])
}

// --- 2. FUNZIONE NOTIFICA LIVE (Chiamata dal Backend Rust) ---
pub async fn send_opportunity_alert(
    bot: &Bot,
    chat_id: ChatId,
    state: &Arc<BotState>, 
    token_address: &str,
    token_symbol: &str,
    safety_score: u8,
    liquidity_usd: f64,
) -> ResponseResult<()> {
    
    // Recupera saldo per calcolare i tasti intelligenti
    let user_id = chat_id.to_string();
    let mut balance_sol = 0.0;
    
    if let Ok(pubkey_str) = crate::wallet_manager::create_user_wallet(&state.pool, &user_id).await {
        if let Ok(pubkey) = Pubkey::from_str(&pubkey_str) {
            let bal = state.network.get_balance_fast(&pubkey).await;
            balance_sol = bal as f64 / LAMPORTS_PER_SOL as f64;
        }
    }

    // Calcolo Strategico dell'Importo
    // "Small" = Cippino di prova
    // "Medium" = Posizione seria (o metà wallet se povero)
    let safe_balance = (balance_sol - 0.02).max(0.0);
    let amount_small = if safe_balance > 0.2 { 0.1 } else { 0.01 };
    let amount_medium = if safe_balance > 1.0 { 0.5 } else { safe_balance * 0.5 };

    let safety_icon = if safety_score > 85 { "🟢" } else if safety_score > 50 { "🟡" } else { "🔴" };

    let text = format!(
        "🚨 <b>GEMMA RILEVATA!</b>\n\n\
        💎 <b>{}</b>\n\
        📜 <code>{}</code>\n\n\
        🛡️ Sicurezza: {} {}/100\n\
        💧 Liquidità: ${:.0}\n\
        💰 Tuo Saldo: {:.3} SOL\n\n\
        <i>Scegli azione immediata o analizza:</i>",
        token_symbol, token_address, safety_icon, safety_score, liquidity_usd, balance_sol
    );

    // DEEP LINK: Apre la Web App direttamente sulla pagina del token specifico
    let app_deep_link = format!("{}/?startapp={}", WEB_APP_URL, token_address);

    let keyboard = InlineKeyboardMarkup::new(vec![
        vec![
            InlineKeyboardButton::web_app(
                format!("📱 GRAFICO & TRADE {}", token_symbol).as_str(), 
                WebAppInfo { url: app_deep_link.parse().unwrap() }
            ),
        ],
        vec![
            InlineKeyboardButton::callback(
                format!("⚡ Compra {:.2} SOL", amount_small).as_str(), 
                format!("buy:{}:{}", token_address, amount_small)
            ),
            InlineKeyboardButton::callback(
                format!("🚀 Compra {:.2} SOL", amount_medium).as_str(), 
                format!("buy:{}:{}", token_address, amount_medium)
            ),
        ],
        vec![InlineKeyboardButton::callback("❌ Ignora", "ignore")]
    ]);

    bot.send_message(chat_id, text)
        .reply_markup(keyboard)
        .parse_mode(ParseMode::Html)
        .await?;

    Ok(())
}

// --- 3. AVVIO BOT (Entry Point) ---
pub async fn start_bot(pool: SqlitePool, network: Arc<NetworkClient>) {
    let bot = Bot::from_env();
    let state = Arc::new(BotState { pool, network });

    let handler = Update::filter_message()
        .filter_command::<Command>()
        .endpoint(answer_command);

    let callback_handler = Update::filter_callback_query()
        .endpoint(answer_callback);

    log::info!("🤖 TELEGRAM UI AVVIATA! (Web App Link: {})", WEB_APP_URL);

    Dispatcher::builder(bot, dptree::entry().branch(handler).branch(callback_handler))
        .dependencies(dptree::deps![state])
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;
}

// --- 4. GESTIONE COMANDI TESTUALI ---
async fn answer_command(bot: Bot, msg: Message, cmd: Command, state: Arc<BotState>) -> ResponseResult<()> {
    match cmd {
        Command::Start => {
            let user_id = msg.chat.id.to_string();
            
            // Crea o Recupera il Wallet
            let wallet_res = crate::wallet_manager::create_user_wallet(&state.pool, &user_id).await;

            let text = match wallet_res {
                Ok(pubkey) => format!(
                    "💎 <b>GOD SNIPER WALLET</b>\n\n\
                    Il tuo terminale di trading istituzionale è pronto.\n\n\
                    🔑 <b>Address:</b> <code>{}</code>\n\
                    🟢 <b>Stato Sistema:</b> ONLINE\n\
                    🤖 <b>Modalità:</b> Ibrida (App + Bot Automatico)\n\n\
                    Clicca sotto per iniziare.",
                    pubkey
                ),
                Err(e) => format!("❌ Errore Critico Creazione Wallet: {}", e),
            };

            bot.send_message(msg.chat.id, text)
                .reply_markup(make_main_keyboard())
                .parse_mode(ParseMode::Html)
                .await?;
        }
        Command::Buy(_) => {
            bot.send_message(msg.chat.id, "⚠️ Per comprare usa i pulsanti rapidi o la Web App per maggiore sicurezza.").await?;
        }
    }
    Ok(())
}

// --- 5. GESTIONE CLICK PULSANTI (Logica Completa) ---
async fn answer_callback(bot: Bot, q: CallbackQuery, state: Arc<BotState>) -> ResponseResult<()> {
    if let Some(data) = q.data {
        let user_id = q.from.id.to_string();
        // Ottieni chat_id in modo sicuro
        let chat_id = if let Some(msg) = &q.message {
            msg.chat.id
        } else {
            // Fallback raro se il messaggio è troppo vecchio
            return Ok(());
        };

        let parts: Vec<&str> = data.split(':').collect();
        let action = parts[0];

        match action {
            // --- A. CONTROLLO AUTO-BOT (DB + Logica) ---
            "start_auto_bot" => {
                match crate::db::start_daily_cycle(&state.pool, &user_id).await {
                    Ok(_) => {
                        bot.send_message(chat_id, "🤖 <b>AUTO-TRADING AVVIATO (24h)</b> 🟢\n\nIl bot cercherà gemme e reinvestirà i profitti.\n⚠️ Prelievi bloccati fino a fine ciclo per compounding.\nPuoi sempre fare trading manuale!").parse_mode(ParseMode::Html).await?;
                    },
                    Err(e) => { bot.send_message(chat_id, format!("Errore Database: {}", e)).await?; }
                }
            },
            "stop_auto_bot" => {
                // Query diretta per spegnere il flag
                sqlx::query("UPDATE users SET is_active = 0 WHERE tg_id = ?")
                    .bind(&user_id)
                    .execute(&state.pool).await.ok();
                bot.send_message(chat_id, "🛑 <b>Auto-Trading Fermato.</b>\nIl bot non comprerà più autonomamente.\nPrelievi sbloccati.").parse_mode(ParseMode::Html).await?;
            },

            // --- B. TRADING MANUALE (Raydium Swap) ---
            "buy" => {
                if parts.len() < 3 { return Ok(()); }
                let token_address = parts[1];
                let amount_sol: f64 = parts[2].parse().unwrap_or(0.01);

                bot.send_message(chat_id, format!("⏳ <b>Esecuzione Swap...</b>\nTarget: <code>{}</code>\nImporto: {} SOL", token_address, amount_sol))
                   .parse_mode(ParseMode::Html).await?;

                // Recupera chiave privata (Decriptata al volo)
                let payer = crate::wallet_manager::get_decrypted_wallet(&state.pool, &user_id).await.unwrap();
                let token_mint = Pubkey::from_str(token_address).unwrap();
                let amount_lamports = (amount_sol * LAMPORTS_PER_SOL as f64) as u64;

                // 1. Trova Pool Keys
                let pool_keys = match crate::raydium::fetch_pool_keys_by_mint(&state.network, &token_mint).await {
                    Ok(k) => k,
                    Err(_) => { bot.send_message(chat_id, "❌ Liquidità non trovata o pool inesistente.").await?; return Ok(()); }
                };

                // 2. Esegui Swap
                // Min Amount 0 per slippage dinamico (massima velocità)
                match crate::raydium::execute_swap(&state.network, &payer, &pool_keys, token_mint, amount_lamports, 0).await {
                    Ok(sig) => {
                         // Salva il Trade nel DB per il P&L
                         let _ = crate::db::record_buy(&state.pool, &user_id, token_address, &sig, amount_lamports).await;
                         
                         let text = format!("✅ <b>ACQUISTO COMPLETATO!</b>\n💎 Token in wallet.\n🔗 <a href=\"https://solscan.io/tx/{}\">Vedi su Solscan</a>", sig);
                         
                         // Tasto per vendere subito
                         let kb = InlineKeyboardMarkup::new(vec![vec![
                             InlineKeyboardButton::callback("🔴 VENDI TUTTO (Panic)", format!("sell:{}:100", token_address))
                         ]]);
                         bot.send_message(chat_id, text).reply_markup(kb).parse_mode(ParseMode::Html).await?;
                    },
                    Err(e) => { bot.send_message(chat_id, format!("❌ Errore Swap: {}", e)).await?; }
                }
            },

            "sell" => {
                let token = parts[1];
                bot.send_message(chat_id, format!("⚠️ Funzione Vendita Manuale per {} in arrivo nel prossimo update...", token)).await?;
                // Qui andrà la chiamata a execute_sell
            },

            // --- C. GESTIONE FONDI (Prelievo con Blocco) ---
            "withdraw_all" => {
                match crate::db::can_withdraw(&state.pool, &user_id).await {
                    Ok((true, _)) => {
                        bot.send_message(chat_id, "💸 <b>Prelievo Sbloccato</b>\n\nPer sicurezza, inserisci l'indirizzo di destinazione nel prossimo messaggio (Funzione in arrivo).").parse_mode(ParseMode::Html).await?;
                    },
                    Ok((false, msg)) => {
                        // Se bloccato, mostra popup alert invece di messaggio
                        bot.answer_callback_query(q.id).text(msg).show_alert(true).await?;
                    },
                    Err(_) => {}
                }
            },
            
            "balance" | "refresh_home" => {
                if let Ok(pubkey_str) = crate::wallet_manager::create_user_wallet(&state.pool, &user_id).await {
                    let pubkey = Pubkey::from_str(&pubkey_str).unwrap();
                    let bal = state.network.get_balance_fast(&pubkey).await as f64 / LAMPORTS_PER_SOL as f64;
                    
                    let text = format!(
                        "💰 <b>Il tuo Portafoglio</b>\n\nIndirizzo:\n<code>{}</code>\n\nSaldo Attuale:\n<b>{:.4} SOL</b>", 
                        pubkey_str, bal
                    );
                    
                    if let Some(msg) = q.message {
                        bot.edit_message_text(msg.chat.id, msg.id, text)
                           .reply_markup(make_main_keyboard())
                           .parse_mode(ParseMode::Html).await?;
                    }
                }
            },
            
            "ignore" => { 
                if let Some(msg) = q.message { 
                    bot.delete_message(msg.chat.id, msg.id).await?; 
                } 
            },
            
            _ => {}
        }
    }
    Ok(())
}