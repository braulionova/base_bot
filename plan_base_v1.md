# Plan Base V1 — Longtail MEV Bot

## Estado Actual
- 43,658 pools indexados, 18 DEXes
- ML Engine: 4 modelos (PoolScorer, TradeSizer, SuccessPredictor, Timing)
- Event Hunter corriendo (detecta swaps, mints, burns)
- Contrato V2 flash swap deployado
- Nodo Base sincronizando (~53% Bodies)
- Wallet: 0 ETH (blocker)

## Implementación Tier 2

### T2.1 — Contrato V3↔V3 via Aave Flash Loan
- Aave presta tokens → swap poolA → swap poolB → repay
- Transfer tokens a pool ANTES de llamar swap (evita callback deadlock)
- Soporte V2 y V3 en ambos legs

### T2.2 — WebSocket Real-Time
- Suscripción a newHeads + logs via ws://localhost:8546
- Fallback a polling cuando WS no disponible
- Procesar eventos instant en vez de polling cada 2s

### T2.3 — Multi-hop A→B→C→A (Triangular)
- WETH→TokenA→TokenB→WETH via 3 pools
- 3x más rutas posibles
- ML prioriza triangulares con mayor spread

### T2.4 — Token Blacklist ML
- Modelo aprende de honeypots/reverts
- Features: code size, age, holder count, transfer tax
- Auto-blacklist tokens que siempre revertan

### T2.5 — Gas Price Predictor
- Predice gas óptimo por hora del día
- Evita overpay en bloques baratos
- Sube gas en momentos competitivos

### T2.6 — Ejecución Concurrente
- Múltiples arbs en paralelo (diferentes nonces)
- Queue de txs pendientes
- Nonce manager thread-safe

### T2.7 — P&L Tracker + Auto-Withdraw
- Registro de cada tx: gas, profit, token, pool
- Dashboard en Telegram cada hora
- Auto-withdraw cuando profit > threshold
