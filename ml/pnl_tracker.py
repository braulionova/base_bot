"""
P&L Tracker — Records every tx, tracks profit by strategy/pool/token/hour.
Sends hourly summary to Telegram.
"""
import json
import os
import time
from collections import defaultdict

PNL_FILE = "/root/arb-flash-bot/ml/pnl_history.json"


class PnLTracker:
    def __init__(self):
        self.trades = []
        self.total_gas_spent = 0.0
        self.total_revenue = 0.0
        self.by_strategy = defaultdict(lambda: {"wins": 0, "fails": 0, "revenue": 0.0, "gas": 0.0})
        self.by_hour = defaultdict(lambda: {"wins": 0, "fails": 0, "revenue": 0.0, "gas": 0.0})
        self.by_token = defaultdict(lambda: {"wins": 0, "fails": 0, "revenue": 0.0})
        self.load()

    def record(self, tx_hash, strategy, token, pool_a, pool_b, amount, gas_cost_usd, revenue_usd, success):
        trade = {
            "time": time.time(),
            "tx": tx_hash,
            "strategy": strategy,
            "token": token[:16],
            "pool_a": pool_a[:16],
            "pool_b": pool_b[:16],
            "amount": amount,
            "gas_usd": gas_cost_usd,
            "revenue_usd": revenue_usd,
            "success": success,
            "net_usd": revenue_usd - gas_cost_usd if success else -gas_cost_usd
        }
        self.trades.append(trade)

        hour = time.gmtime().tm_hour
        s = self.by_strategy[strategy]
        h = self.by_hour[hour]
        t = self.by_token[token[:16]]

        gas = gas_cost_usd
        self.total_gas_spent += gas

        if success:
            s["wins"] += 1; h["wins"] += 1; t["wins"] += 1
            s["revenue"] += revenue_usd; h["revenue"] += revenue_usd; t["revenue"] += revenue_usd
            self.total_revenue += revenue_usd
        else:
            s["fails"] += 1; h["fails"] += 1; t["fails"] += 1

        s["gas"] += gas; h["gas"] += gas

        # Auto-save every 10 trades
        if len(self.trades) % 10 == 0:
            self.save()

    def net_profit(self):
        return self.total_revenue - self.total_gas_spent

    def summary(self):
        n = len(self.trades)
        wins = sum(1 for t in self.trades if t["success"])
        net = self.net_profit()
        return (f"Trades: {n} | Wins: {wins} | "
                f"Revenue: ${self.total_revenue:.4f} | Gas: ${self.total_gas_spent:.4f} | "
                f"Net: ${net:.4f}")

    def hourly_summary(self):
        lines = []
        for h in sorted(self.by_hour.keys()):
            d = self.by_hour[h]
            net = d["revenue"] - d["gas"]
            lines.append(f"{h:02d}:00 W:{d['wins']} F:{d['fails']} ${net:.3f}")
        return "\n".join(lines)

    def strategy_summary(self):
        lines = []
        for s, d in self.by_strategy.items():
            net = d["revenue"] - d["gas"]
            wr = d["wins"] / max(d["wins"] + d["fails"], 1) * 100
            lines.append(f"{s}: W:{d['wins']} F:{d['fails']} WR:{wr:.0f}% ${net:.3f}")
        return "\n".join(lines)

    def save(self):
        data = {
            "trades": self.trades[-1000:],  # Keep last 1000
            "total_gas": self.total_gas_spent,
            "total_revenue": self.total_revenue,
        }
        with open(PNL_FILE, "w") as f:
            json.dump(data, f)

    def load(self):
        try:
            with open(PNL_FILE) as f:
                data = json.load(f)
                self.trades = data.get("trades", [])
                self.total_gas_spent = data.get("total_gas", 0)
                self.total_revenue = data.get("total_revenue", 0)
        except:
            pass
