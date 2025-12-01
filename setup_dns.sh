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
echo "→ Test quote-api.jup.ag (IPv4)..."
if host -4 quote-api.jup.ag 1.1.1.1 2>/dev/null | grep -q "has address"; then
    IP=$(host -4 quote-api.jup.ag 1.1.1.1 2>/dev/null | grep "has address" | head -1 | awk '{print $NF}')
    echo "  ✅ quote-api.jup.ag → $IP"
else
    echo "  ❌ Errore risoluzione quote-api.jup.ag"
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
    echo "  ❌ Errore risoluzione token.jup.ag"
fi

# Test connessione HTTPS
echo ""
echo "→ Test HTTPS quote-api.jup.ag..."
if curl -4 -s --connect-timeout 5 "https://quote-api.jup.ag/v6/quote?inputMint=So11111111111111111111111111111111111111112&outputMint=EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v&amount=1000000&slippageBps=50" > /dev/null 2>&1; then
    echo "  ✅ Jupiter API raggiungibile!"
else
    echo "  ❌ Jupiter API non raggiungibile"
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
