#!/bin/bash
# ═══════════════════════════════════════════════════════════════════════════════
# SETUP DNS + IPv4 OTTIMIZZATO PER AWS
# Configura Cloudflare (1.1.1.1) e Google (8.8.8.8) come DNS primari
# DISABILITA IPv6 - Jupiter API non supporta IPv6 (errore AAAA record)
# ═══════════════════════════════════════════════════════════════════════════════

echo "🔧 Configurazione DNS + IPv4 per AWS..."
echo ""

# ═══════════════════════════════════════════════════════════════════════════════
# STEP 1: DISABILITA IPv6 (FIX PER ERRORE AAAA JUPITER)
# ═══════════════════════════════════════════════════════════════════════════════
echo "📌 Step 1: Disabilitazione IPv6..."

# Disabilita IPv6 via sysctl
sudo sysctl -w net.ipv6.conf.all.disable_ipv6=1 > /dev/null 2>&1
sudo sysctl -w net.ipv6.conf.default.disable_ipv6=1 > /dev/null 2>&1
sudo sysctl -w net.ipv6.conf.lo.disable_ipv6=1 > /dev/null 2>&1

# Rendi permanente
if ! grep -q "net.ipv6.conf.all.disable_ipv6" /etc/sysctl.conf 2>/dev/null; then
    echo "" | sudo tee -a /etc/sysctl.conf > /dev/null
    echo "# Disabilita IPv6 - Fix per Jupiter API (no AAAA record)" | sudo tee -a /etc/sysctl.conf > /dev/null
    echo "net.ipv6.conf.all.disable_ipv6 = 1" | sudo tee -a /etc/sysctl.conf > /dev/null
    echo "net.ipv6.conf.default.disable_ipv6 = 1" | sudo tee -a /etc/sysctl.conf > /dev/null
    echo "net.ipv6.conf.lo.disable_ipv6 = 1" | sudo tee -a /etc/sysctl.conf > /dev/null
fi

echo "  ✅ IPv6 disabilitato"

# ═══════════════════════════════════════════════════════════════════════════════
# STEP 2: CONFIGURA DNS (Cloudflare + Google)
# ═══════════════════════════════════════════════════════════════════════════════
echo "📌 Step 2: Configurazione DNS..."

# Backup del file originale
if [ -f /etc/resolv.conf ]; then
    sudo cp /etc/resolv.conf /etc/resolv.conf.backup 2>/dev/null
fi

# Previeni che il file venga sovrascritto da DHCP
sudo chattr -i /etc/resolv.conf 2>/dev/null || true

# Scrivi nuova configurazione DNS (SOLO IPv4)
cat << 'EOF' | sudo tee /etc/resolv.conf > /dev/null
# DNS Configuration - Ottimizzato per AWS + IPv4 Only
# Cloudflare DNS (più veloce, privacy-first)
nameserver 1.1.1.1
nameserver 1.0.0.1
# Google DNS (backup affidabile)
nameserver 8.8.8.8
nameserver 8.8.4.4
# Opzioni ottimizzate - SOLO IPv4
options timeout:2 attempts:3 rotate single-request-reopen
EOF

# Rendi il file immutabile per evitare sovrascritture
sudo chattr +i /etc/resolv.conf 2>/dev/null || true

echo "  ✅ DNS configurati (1.1.1.1, 8.8.8.8)"

# ═══════════════════════════════════════════════════════════════════════════════
# STEP 3: TEST CONNETTIVITÀ (SOLO IPv4)
# ═══════════════════════════════════════════════════════════════════════════════
echo ""
echo "📌 Step 3: Test connettività IPv4..."
echo ""

# Test DNS resolution (forza IPv4 con -4)
# NOTA: quote-api.jup.ag NON ha record A (IPv4)! Usiamo lite-api.jup.ag
echo "→ Test lite-api.jup.ag (IPv4) - Jupiter Swap API..."
if host -4 lite-api.jup.ag 1.1.1.1 2>/dev/null | grep -q "has address"; then
    IP=$(host -4 lite-api.jup.ag 1.1.1.1 2>/dev/null | grep "has address" | head -1 | awk '{print $NF}')
    echo "  ✅ lite-api.jup.ag → $IP"
else
    echo "  ❌ Errore risoluzione lite-api.jup.ag"
fi

echo "→ Test api.dexscreener.com (IPv4)..."
if host -4 api.dexscreener.com 1.1.1.1 2>/dev/null | grep -q "has address"; then
    IP=$(host -4 api.dexscreener.com 1.1.1.1 2>/dev/null | grep "has address" | head -1 | awk '{print $NF}')
    echo "  ✅ api.dexscreener.com → $IP"
else
    echo "  ❌ Errore risoluzione api.dexscreener.com"
fi

echo "→ Test token.jup.ag (IPv4)..."
if host -4 token.jup.ag 1.1.1.1 2>/dev/null | grep -q "has address"; then
    IP=$(host -4 token.jup.ag 1.1.1.1 2>/dev/null | grep "has address" | head -1 | awk '{print $NF}')
    echo "  ✅ token.jup.ag → $IP"
else
    # token.jup.ag potrebbe non avere A record, non è critico
    echo "  ⚠️ token.jup.ag non ha record A (normale)"
fi

# Test connessione HTTPS - USA lite-api.jup.ag!
echo ""
echo "→ Test HTTPS lite-api.jup.ag (Jupiter Swap API)..."
RESPONSE=$(curl -4 -s --connect-timeout 5 "https://lite-api.jup.ag/swap/v1/quote?inputMint=So11111111111111111111111111111111111111112&outputMint=EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v&amount=1000000&slippageBps=50" 2>&1)
if echo "$RESPONSE" | grep -q "outAmount"; then
    AMOUNT=$(echo "$RESPONSE" | grep -o '"outAmount":"[0-9]*"' | head -1 | cut -d'"' -f4)
    echo "  ✅ Jupiter API OK! (1 SOL ≈ $AMOUNT USDC)"
else
    echo "  ❌ Jupiter API errore: $RESPONSE"
fi

echo ""
echo "═══════════════════════════════════════════════════════════════"
echo "✅ Configurazione completata!"
echo ""
echo "Riepilogo:"
echo "  • IPv6: DISABILITATO (fix errore AAAA)"
echo "  • DNS: Cloudflare (1.1.1.1) + Google (8.8.8.8)"
echo "  • Protocollo: Solo IPv4"
echo ""
echo "Per annullare le modifiche:"
echo "  sudo chattr -i /etc/resolv.conf"
echo "  sudo cp /etc/resolv.conf.backup /etc/resolv.conf"
echo "  sudo sysctl -w net.ipv6.conf.all.disable_ipv6=0"
echo "═══════════════════════════════════════════════════════════════"
