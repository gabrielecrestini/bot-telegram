#!/bin/bash
# ═══════════════════════════════════════════════════════════════════════════════
# SETUP DNS OTTIMIZZATO PER AWS
# Configura Cloudflare (1.1.1.1) e Google (8.8.8.8) come DNS primari
# Risolve problemi di connettività DNS su istanze AWS EC2
# ═══════════════════════════════════════════════════════════════════════════════

echo "🔧 Configurazione DNS per AWS..."

# Backup del file originale
if [ -f /etc/resolv.conf ]; then
    sudo cp /etc/resolv.conf /etc/resolv.conf.backup
    echo "✓ Backup creato: /etc/resolv.conf.backup"
fi

# Previeni che il file venga sovrascritto da DHCP
sudo chattr -i /etc/resolv.conf 2>/dev/null || true

# Scrivi nuova configurazione DNS
cat << 'EOF' | sudo tee /etc/resolv.conf > /dev/null
# DNS Configuration - Ottimizzato per AWS
# Cloudflare DNS (più veloce, privacy-first)
nameserver 1.1.1.1
nameserver 1.0.0.1
# Google DNS (backup affidabile)
nameserver 8.8.8.8
nameserver 8.8.4.4
# Opzioni ottimizzate
options timeout:2 attempts:3 rotate
EOF

echo "✓ DNS configurati:"
echo "  - 1.1.1.1 (Cloudflare primario)"
echo "  - 1.0.0.1 (Cloudflare secondario)"
echo "  - 8.8.8.8 (Google primario)"
echo "  - 8.8.4.4 (Google secondario)"

# Rendi il file immutabile per evitare sovrascritture
sudo chattr +i /etc/resolv.conf 2>/dev/null || true

echo ""
echo "🔍 Test connettività DNS..."
echo ""

# Test DNS resolution
echo "→ Test quote-api.jup.ag..."
if nslookup quote-api.jup.ag > /dev/null 2>&1; then
    echo "  ✅ quote-api.jup.ag risolto"
else
    echo "  ❌ Errore risoluzione quote-api.jup.ag"
fi

echo "→ Test api.dexscreener.com..."
if nslookup api.dexscreener.com > /dev/null 2>&1; then
    echo "  ✅ api.dexscreener.com risolto"
else
    echo "  ❌ Errore risoluzione api.dexscreener.com"
fi

echo "→ Test token.jup.ag..."
if nslookup token.jup.ag > /dev/null 2>&1; then
    echo "  ✅ token.jup.ag risolto"
else
    echo "  ❌ Errore risoluzione token.jup.ag"
fi

echo ""
echo "═══════════════════════════════════════════════════════════════"
echo "✅ Configurazione DNS completata!"
echo ""
echo "Per annullare le modifiche:"
echo "  sudo chattr -i /etc/resolv.conf"
echo "  sudo cp /etc/resolv.conf.backup /etc/resolv.conf"
echo "═══════════════════════════════════════════════════════════════"
