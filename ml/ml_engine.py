#!/usr/bin/env python3
"""
ML Engine for Longtail MEV
==========================
4 models that plug into the hunter pipeline:

1. POOL SCORER: Rank pools by profit potential (which pools to watch)
2. TRADE SIZER: Optimal borrow amount given reserves/spread
3. SUCCESS PREDICTOR: Will this sim-passing arb succeed on-chain? (avoid gas waste)
4. TIMING MODEL: When are arbs most profitable? (hour/block patterns)

Trains on pool data + on-chain observations. Updates continuously.
"""

import json
import numpy as np
import os
import pickle
import time
from collections import defaultdict
from sklearn.ensemble import GradientBoostingClassifier, GradientBoostingRegressor
from sklearn.preprocessing import StandardScaler

WETH = "0x4200000000000000000000000000000000000006".lower()
USDC = "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913".lower()
MODEL_DIR = "/root/arb-flash-bot/ml/models"
os.makedirs(MODEL_DIR, exist_ok=True)


class PoolScorer:
    """Model 1: Score pools by profit potential.
    Features: liquidity, swap frequency, fee tier, num sisters, token age.
    Target: expected profit per arb attempt.
    """
    def __init__(self):
        self.model = None
        self.scaler = StandardScaler()
        self.pool_scores = {}

    def extract_features(self, pools_data):
        """Extract features from pool cache + active pools data"""
        from collections import Counter

        pairs = defaultdict(list)
        token_freq = Counter()
        dex_count = Counter()

        for p in pools_data:
            t0, t1 = p["token0"].lower(), p["token1"].lower()
            k = (min(t0, t1), max(t0, t1))
            pairs[k].append(p)
            token_freq[t0] += 1
            token_freq[t1] += 1
            dex_count[p["dex_name"]] += 1

        features = []
        addresses = []

        for p in pools_data:
            t0, t1 = p["token0"].lower(), p["token1"].lower()
            k = (min(t0, t1), max(t0, t1))

            has_weth = WETH in (t0, t1)
            has_stable = USDC in (t0, t1)
            longtail = t1 if t0 == WETH else t0
            num_sisters = len(pairs[k]) - 1
            token_popularity = token_freq.get(longtail, 1)
            is_v2 = 1 if p["pool_type"] == "V2" else 0
            fee = p["fee"]
            comp = p.get("competition_score", 0)
            comp = comp if comp < 2**60 else 0

            # Sisters with different pool types (V2+V3 = arb opportunity)
            sister_types = set(s["pool_type"] for s in pairs[k])
            has_mixed_types = 1 if len(sister_types) >= 2 else 0

            # Sisters on different DEXes
            sister_dexes = set(s["dex_name"] for s in pairs[k])
            num_dexes = len(sister_dexes)

            f = [
                1 if has_weth else 0,
                1 if has_stable else 0,
                num_sisters,
                1.0 / max(token_popularity, 1),  # obscurity
                is_v2,
                fee / 10000.0,
                min(comp, 100),
                has_mixed_types,
                num_dexes,
                1 if num_sisters > 0 and has_weth else 0,  # arbable WETH pair
            ]
            features.append(f)
            addresses.append(p["address"])

        return np.array(features), addresses

    def train(self, pools_data):
        """Train on pool features. Label: profit potential score."""
        X, addrs = self.extract_features(pools_data)
        if len(X) == 0:
            return

        # Synthetic labels based on heuristics (bootstrap model)
        y = np.zeros(len(X))
        for i, f in enumerate(X):
            score = 0
            if f[0]:  # has_weth
                score += 3
            if f[2] > 0:  # has sisters
                score += 2
            if f[7]:  # mixed types
                score += 3
            if f[3] > 0.1:  # obscure token
                score += 2
            if f[4]:  # is V2 (flash source)
                score += 1
            if f[8] >= 2:  # multi-dex
                score += 2
            if f[9]:  # arbable WETH
                score += 5
            y[i] = min(score / 18.0, 1.0)

        self.scaler.fit(X)
        Xs = self.scaler.transform(X)

        self.model = GradientBoostingRegressor(
            n_estimators=100, max_depth=4, learning_rate=0.1, random_state=42
        )
        self.model.fit(Xs, y)

        # Score all pools
        scores = self.model.predict(Xs)
        for addr, score in zip(addrs, scores):
            self.pool_scores[addr.lower()] = float(score)

        # Save
        with open(f"{MODEL_DIR}/pool_scorer.pkl", "wb") as f:
            pickle.dump({"model": self.model, "scaler": self.scaler}, f)

        top = sorted(self.pool_scores.items(), key=lambda x: -x[1])[:10]
        print(f"Pool Scorer trained on {len(X)} pools")
        print(f"Top 10 pools: {[f'{a[:10]}..={s:.2f}' for a,s in top]}")
        return self.pool_scores

    def get_score(self, addr):
        return self.pool_scores.get(addr.lower(), 0.0)

    def get_top_pools(self, n=100):
        return sorted(self.pool_scores.items(), key=lambda x: -x[1])[:n]


class TradeSizer:
    """Model 2: Optimal borrow amount given pool reserves.
    Too small = profit < gas. Too large = slippage eats profit.
    Sweet spot depends on liquidity depth.
    """
    def __init__(self):
        self.model = None

    def optimal_amount(self, reserve_borrow, reserve_pay, fee_bps=30):
        """Calculate optimal borrow amount for V2 flash arb.
        Based on AMM math: optimal is when marginal profit = marginal slippage cost.
        For V2: optimal ≈ sqrt(reserve * spread_amount) for small spreads.
        Simplified: borrow 1-3% of reserve for longtail pools.
        """
        if reserve_borrow < 1000000:
            return 0

        # Start with 2% of reserve
        base_pct = 0.02

        # Adjust based on liquidity depth
        if reserve_borrow > 10**18:  # >1 ETH equivalent
            base_pct = 0.03  # Can be more aggressive
        elif reserve_borrow > 10**16:  # >0.01 ETH
            base_pct = 0.02
        elif reserve_borrow > 10**14:  # >0.0001 ETH
            base_pct = 0.01
        else:
            base_pct = 0.005

        amount = int(reserve_borrow * base_pct)

        # Don't borrow more than 5% ever (diminishing returns)
        max_amount = int(reserve_borrow * 0.05)
        amount = min(amount, max_amount)

        return max(amount, 100000)  # min 100k wei

    def multi_size_attempts(self, reserve_borrow, reserve_pay):
        """Return list of amounts to try, ordered by expected profit."""
        base = self.optimal_amount(reserve_borrow, reserve_pay)
        if base == 0:
            return []
        return [
            base,
            int(base * 0.5),
            int(base * 2),
            int(base * 0.2),
        ]


class SuccessPredictor:
    """Model 3: Predict if a sim-passing arb will succeed on-chain.
    Learns from execution history: sim passed but tx reverted = feature pattern.
    Key features: time since last block, gas price, pool activity level.
    """
    def __init__(self):
        self.model = None
        self.scaler = StandardScaler()
        self.history = []  # (features, success) pairs

    def add_result(self, features, success):
        """Record an execution result for training."""
        self.history.append((features, 1 if success else 0))
        if len(self.history) >= 50 and len(self.history) % 10 == 0:
            self._retrain()

    def predict(self, features):
        """Predict success probability. Returns 0-1."""
        if self.model is None:
            return 0.5  # no data yet, 50/50

        Xs = self.scaler.transform([features])
        return float(self.model.predict_proba(Xs)[0][1])

    def _retrain(self):
        X = np.array([h[0] for h in self.history])
        y = np.array([h[1] for h in self.history])
        if sum(y) == 0 or sum(y) == len(y):
            return  # need both classes

        self.scaler.fit(X)
        Xs = self.scaler.transform(X)
        self.model = GradientBoostingClassifier(
            n_estimators=50, max_depth=3, learning_rate=0.1
        )
        self.model.fit(Xs, y)
        acc = self.model.score(Xs, y)
        print(f"Success Predictor retrained: {len(self.history)} samples, acc={acc:.2f}")

    def make_features(self, pool_score, reserve_ratio, num_sisters, seconds_since_event, swap_count_recent):
        """Create feature vector for prediction."""
        return [
            pool_score,
            reserve_ratio,
            num_sisters,
            seconds_since_event,
            swap_count_recent,
            time.time() % 86400 / 86400,  # time of day normalized
            int(time.time()) % 7 / 7,  # day of week normalized
        ]


class TimingModel:
    """Model 4: When are arbs most profitable?
    Tracks profit by hour/day, adjusts scan frequency.
    """
    def __init__(self):
        self.hourly_profits = defaultdict(list)
        self.hourly_attempts = defaultdict(int)

    def record(self, profit_usd, success):
        hour = time.gmtime().tm_hour
        self.hourly_profits[hour].append(profit_usd if success else -0.004)
        self.hourly_attempts[hour] += 1

    def best_hours(self):
        """Return hours ranked by average profit."""
        avgs = {}
        for h, profits in self.hourly_profits.items():
            if len(profits) >= 5:
                avgs[h] = np.mean(profits)
        return sorted(avgs.items(), key=lambda x: -x[1])

    def should_scan_aggressively(self):
        """Should we scan more frequently right now?"""
        hour = time.gmtime().tm_hour
        if hour not in self.hourly_profits:
            return True  # unknown = explore
        avg = np.mean(self.hourly_profits[hour])
        return avg > 0  # positive average = good time

    def get_scan_interval(self):
        """Dynamic scan interval based on time of day."""
        if self.should_scan_aggressively():
            return 2  # 2 seconds
        return 5  # 5 seconds during slow periods


class MLEngine:
    """Combined ML engine for the hunter."""

    def __init__(self):
        self.pool_scorer = PoolScorer()
        self.trade_sizer = TradeSizer()
        self.success_pred = SuccessPredictor()
        self.timing = TimingModel()
        self.trained = False

    def train(self, pools_data):
        """Train all models on pool data."""
        scores = self.pool_scorer.train(pools_data)
        self.trained = True
        return scores

    def score_pool(self, addr):
        return self.pool_scorer.get_score(addr)

    def get_priority_pools(self, n=100):
        return self.pool_scorer.get_top_pools(n)

    def optimal_amounts(self, reserve_borrow, reserve_pay):
        return self.trade_sizer.multi_size_attempts(reserve_borrow, reserve_pay)

    def predict_success(self, pool_addr, reserve_ratio, num_sisters, seconds_since_event, swap_count):
        score = self.score_pool(pool_addr)
        features = self.success_pred.make_features(score, reserve_ratio, num_sisters, seconds_since_event, swap_count)
        return self.success_pred.predict(features)

    def record_execution(self, pool_addr, reserve_ratio, num_sisters, seconds_since_event, swap_count, success, profit_usd=0):
        score = self.score_pool(pool_addr)
        features = self.success_pred.make_features(score, reserve_ratio, num_sisters, seconds_since_event, swap_count)
        self.success_pred.add_result(features, success)
        self.timing.record(profit_usd, success)

    def get_scan_interval(self):
        return self.timing.get_scan_interval()

    def save(self):
        with open(f"{MODEL_DIR}/ml_engine.pkl", "wb") as f:
            pickle.dump({
                "history": self.success_pred.history,
                "timing": dict(self.timing.hourly_profits),
            }, f)

    def load(self):
        try:
            with open(f"{MODEL_DIR}/ml_engine.pkl", "rb") as f:
                data = pickle.load(f)
                self.success_pred.history = data.get("history", [])
                for h, profits in data.get("timing", {}).items():
                    self.timing.hourly_profits[h] = profits
                if len(self.success_pred.history) >= 50:
                    self.success_pred._retrain()
        except:
            pass


if __name__ == "__main__":
    # Train and show results
    print("Training ML Engine...")

    with open("/root/arb-flash-bot/pools_cache.json") as f:
        pools = json.load(f)

    engine = MLEngine()
    engine.train(pools)

    print(f"\n=== TOP 20 PRIORITY POOLS ===")
    for addr, score in engine.get_priority_pools(20):
        p = next((x for x in pools if x["address"].lower() == addr), None)
        if p:
            print(f"  {score:.3f} | {p['dex_name']:15} | {p['pool_type']} | fee={p['fee']:6} | {addr[:16]}..")

    print(f"\n=== TRADE SIZING EXAMPLES ===")
    for res in [10**14, 10**15, 10**16, 10**17, 10**18]:
        amts = engine.optimal_amounts(res, res)
        print(f"  Reserve {res/10**18:.4f} ETH -> try amounts: {[a/10**18 for a in amts[:3]]}")

    print(f"\n=== TIMING ===")
    print(f"  Scan interval: {engine.get_scan_interval()}s")
    print(f"  Aggressive: {engine.timing.should_scan_aggressively()}")

    engine.save()
    print("\nML Engine saved. Ready for integration.")
