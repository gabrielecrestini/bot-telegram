#!/bin/bash
# Script di diagnostica rete per il bot trading

echo "═══════════════════════════════════════════════════════════════"
echo "🔍 DIAGNOSTICA RETE PER IL BOT TRADING"
echo "═══════════════════════════════════════════════════════════════"

echo ""
echo "1️⃣ Test DNS Resolution..."
echo "   Testing quote-api.jup.ag..."
nslookup quote-api.jup.ag 2>&1 || echo "❌ DNS FAILED for quote-api.jup.ag"

echo ""
echo "   Testing api.dexscreener.com..."
nslookup api.dexscreener.com 2>&1 || echo "❌ DNS FAILED for api.dexscreener.com"

echo ""
echo "2️⃣ Test connectivity with curl..."
echo "   Testing Jupiter API..."
curl -s -o /dev/null -w "HTTP Status: %{http_code}\n" --connect-timeout 10 "https://quote-api.jup.ag/v6/quote?inputMint=So11111111111111111111111111111111111111112&outputMint=EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v&amount=1000000&slippageBps=50" || echo "❌ CURL FAILED"

echo ""
echo "   Testing DexScreener API..."
curl -s -o /dev/null -w "HTTP Status: %{http_code}\n" --connect-timeout 10 "https://api.dexscreener.com/latest/dex/tokens/So11111111111111111111111111111111111111112" || echo "❌ CURL FAILED"

echo ""
echo "3️⃣ Check /etc/resolv.conf..."
cat /etc/resolv.conf

echo ""
echo "4️⃣ Check Internet Gateway (ping Google DNS)..."
ping -c 3 8.8.8.8 2>&1 || echo "❌ No internet connectivity"

echo ""
echo "5️⃣ Check HTTPS outbound (port 443)..."
nc -zv quote-api.jup.ag 443 2>&1 || echo "❌ Port 443 blocked"

echo ""
echo "═══════════════════════════════════════════════════════════════"
echo "📋 POSSIBILI SOLUZIONI:"
echo "═══════════════════════════════════════════════════════════════"
echo ""
echo "Se DNS fallisce:"
echo "   1. Modifica /etc/resolv.conf con DNS pubblici:"
echo "      echo 'nameserver 8.8.8.8' | sudo tee /etc/resolv.conf"
echo "      echo 'nameserver 1.1.1.1' | sudo tee -a /etc/resolv.conf"
echo ""
echo "   2. O configura DHCP per usare DNS Amazon:"
echo "      sudo dhclient -r && sudo dhclient"
echo ""
echo "Se curl fallisce ma ping funziona:"
echo "   - Controlla Security Group AWS: assicurati che OUTBOUND abbia:"
echo "     - All traffic → 0.0.0.0/0 (o almeno HTTPS 443)"
echo ""
echo "Se ping fallisce:"
echo "   - La EC2 non ha accesso internet"
echo "   - Verifica che il VPC abbia un Internet Gateway"
echo "   - Verifica la Route Table (0.0.0.0/0 → IGW)"
echo ""
echo "═══════════════════════════════════════════════════════════════"
