#!/usr/bin/env python3
"""
ML Route Scorer for Longtail Arbitrage
=======================================
Analyzes 43k+ pools, extracts features, trains a model to score routes
by profitability potential and competition level.

Output: prioritized_routes.json consumed by the Rust bot.

Features per route (pool pair):
  - pool_age_blocks: newer pools = less competition
  - liquidity_imbalance: price deviation between pools
  - swap_frequency: fewer recent swaps = less bot activity
  - dex_diversity: how many DEXes list this pair
  - token_obscurity: inverse of how many pools contain this token
  - fee_delta: fee difference between pools (arb opportunity indicator)
  - pool_type_mix: V2+V3 mix = more arb opportunity (different pricing)

Model: GradientBoosting (fast, works well with tabular data)
"""

import json
import sys
import os
import time
from collections import Counter, defaultdict
from itertools import combinations
import numpy as np
import pandas as pd
from sklearn.ensemble import GradientBoostingClassifier, RandomForestClassifier
from sklearn.model_selection import train_test_split
from sklearn.preprocessing import StandardScaler
import pickle

POOL_CACHE = "/root/arb-flash-bot/pools_cache.json"
OUTPUT_FILE = "/root/arb-flash-bot/prioritized_routes.json"
MODEL_FILE = "/root/arb-flash-bot/ml/model.pkl"
WETH = "0x4200000000000000000000000000000000000006".lower()

# Known stablecoins/major tokens (high competition, skip)
HIGH_COMP_TOKENS = {
    "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913",  # USDC
    "0xd9aaec86b65d86f6a7b5b1b0c42ffa531710b6ca",  # USDbC
    "0x50c5725949a6f0c72e6c4a641f24049a917db0cb",  # DAI
    WETH,
}


def load_pools():
    with open(POOL_CACHE) as f:
        pools = json.load(f)
    print(f"Loaded {len(pools)} pools")
    return pools


def extract_features(pools):
    """Extract ML features from pool data."""

    # Index pools by normalized token pair
    pair_pools = defaultdict(list)  # (tokenA, tokenB) -> [pool, ...]
    token_count = Counter()  # token -> number of pools containing it
    dex_count = Counter()  # dex_name -> pool count

    for p in pools:
        t0 = p["token0"].lower()
        t1 = p["token1"].lower()
        key = (min(t0, t1), max(t0, t1))
        pair_pools[key].append(p)
        token_count[t0] += 1
        token_count[t1] += 1
        dex_count[p["dex_name"]] += 1

    print(f"Unique token pairs: {len(pair_pools)}")
    print(f"Unique tokens: {len(token_count)}")
    print(f"DEXes: {dict(dex_count)}")

    # Build routes: pairs with 2+ pools on different DEXes
    routes = []

    for (t0, t1), pool_list in pair_pools.items():
        if len(pool_list) < 2:
            continue

        # Need at least one pool with WETH for profitable arb
        has_weth = t0 == WETH or t1 == WETH

        # Get unique DEXes for this pair
        dexes = set(p["dex_name"] for p in pool_list)
        if len(dexes) < 2:
            continue  # Same DEX, no cross-DEX arb

        # The longtail token (non-WETH)
        longtail_token = t1 if t0 == WETH else t0

        # Skip if both tokens are major (high competition)
        if t0 in HIGH_COMP_TOKENS and t1 in HIGH_COMP_TOKENS:
            continue

        # Features
        pool_types = [p["pool_type"] for p in pool_list]
        fees = [p["fee"] for p in pool_list]
        comp_scores = [p["competition_score"] for p in pool_list if p["competition_score"] < 2**63]

        # Token obscurity: fewer pools = more obscure = less competition
        longtail_popularity = token_count.get(longtail_token, 1)

        for i, pa in enumerate(pool_list):
            for pb in pool_list[i+1:]:
                if pa["dex_name"] == pb["dex_name"]:
                    continue

                route = {
                    "pool_a": pa["address"],
                    "pool_b": pb["address"],
                    "dex_a": pa["dex_name"],
                    "dex_b": pb["dex_name"],
                    "token0": t0,
                    "token1": t1,
                    "has_weth": has_weth,
                    # Features
                    "f_dex_diversity": len(dexes),
                    "f_token_obscurity": 1.0 / max(longtail_popularity, 1),
                    "f_pool_type_mix": 1 if pa["pool_type"] != pb["pool_type"] else 0,
                    "f_fee_delta": abs(pa["fee"] - pb["fee"]),
                    "f_fee_a": pa["fee"],
                    "f_fee_b": pb["fee"],
                    "f_comp_score_a": pa["competition_score"] if pa["competition_score"] < 2**63 else -1,
                    "f_comp_score_b": pb["competition_score"] if pb["competition_score"] < 2**63 else -1,
                    "f_avg_competition": np.mean(comp_scores) if comp_scores else -1,
                    "f_is_longtail": 1 if longtail_popularity <= 5 else 0,
                    "f_has_weth": 1 if has_weth else 0,
                    "f_both_v3": 1 if pa["pool_type"] == "V3" and pb["pool_type"] == "V3" else 0,
                    "f_v3_v2_mix": 1 if (pa["pool_type"] == "V3") != (pb["pool_type"] == "V3") else 0,
                }
                routes.append(route)

    print(f"Total cross-DEX routes: {len(routes)}")
    return routes, pair_pools, token_count


def generate_training_labels(routes):
    """
    Generate synthetic training labels based on heuristics.

    Positive (likely profitable):
    - Has WETH (can arb back to ETH)
    - Low competition score
    - Token is obscure (few pools)
    - Pool type mix (V2/V3 = different pricing = arb opportunity)
    - Fee delta > 0 (different fees = price difference)

    Negative (likely unprofitable):
    - Both tokens are major (high competition)
    - High competition scores
    - Token is popular (many bots watching)
    """
    labels = []

    for r in routes:
        score = 0.0

        # WETH pair = can profit in ETH
        if r["f_has_weth"]:
            score += 2.0

        # Low competition = gold
        if r["f_comp_score_a"] == 0 and r["f_comp_score_b"] == 0:
            score += 3.0
        elif r["f_comp_score_a"] >= 0 and r["f_comp_score_a"] <= 2:
            score += 1.0
        elif r["f_comp_score_a"] > 5:
            score -= 2.0

        # Obscure token = less competition
        if r["f_is_longtail"]:
            score += 2.0
        if r["f_token_obscurity"] > 0.2:  # < 5 pools
            score += 1.0

        # V2/V3 mix = different pricing models = arb
        if r["f_v3_v2_mix"]:
            score += 1.5

        # Fee delta = price difference
        if r["f_fee_delta"] > 0:
            score += 1.0

        # More DEXes = more options
        if r["f_dex_diversity"] >= 3:
            score += 0.5

        # Unknown competition (not analyzed yet) = potential
        if r["f_comp_score_a"] == -1:
            score += 0.5

        labels.append(1 if score >= 4.0 else 0)

    return labels


def train_model(routes, labels):
    """Train gradient boosting model."""
    feature_cols = [
        "f_dex_diversity", "f_token_obscurity", "f_pool_type_mix",
        "f_fee_delta", "f_fee_a", "f_fee_b",
        "f_comp_score_a", "f_comp_score_b", "f_avg_competition",
        "f_is_longtail", "f_has_weth", "f_both_v3", "f_v3_v2_mix"
    ]

    X = pd.DataFrame(routes)[feature_cols].values
    y = np.array(labels)

    print(f"Training data: {len(X)} routes, {sum(y)} positive ({100*sum(y)/len(y):.1f}%)")

    # Handle -1 (unknown) values
    X = np.where(X == -1, 0, X)

    X_train, X_test, y_train, y_test = train_test_split(X, y, test_size=0.2, random_state=42)

    scaler = StandardScaler()
    X_train_s = scaler.fit_transform(X_train)
    X_test_s = scaler.transform(X_test)

    model = GradientBoostingClassifier(
        n_estimators=200,
        max_depth=5,
        learning_rate=0.1,
        min_samples_leaf=10,
        random_state=42
    )
    model.fit(X_train_s, y_train)

    train_acc = model.score(X_train_s, y_train)
    test_acc = model.score(X_test_s, y_test)
    print(f"Model accuracy: train={train_acc:.3f}, test={test_acc:.3f}")

    # Feature importance
    importances = list(zip(feature_cols, model.feature_importances_))
    importances.sort(key=lambda x: -x[1])
    print("Feature importance:")
    for name, imp in importances:
        print(f"  {name}: {imp:.3f}")

    # Score ALL routes
    X_all = np.where(pd.DataFrame(routes)[feature_cols].values == -1, 0,
                     pd.DataFrame(routes)[feature_cols].values)
    X_all_s = scaler.transform(X_all)
    probabilities = model.predict_proba(X_all_s)[:, 1]

    # Save model
    os.makedirs(os.path.dirname(MODEL_FILE), exist_ok=True)
    with open(MODEL_FILE, "wb") as f:
        pickle.dump({"model": model, "scaler": scaler, "features": feature_cols}, f)
    print(f"Model saved to {MODEL_FILE}")

    return probabilities


def prioritize_routes(routes, scores):
    """Sort routes by ML score and output prioritized list."""

    # Add scores to routes
    for i, r in enumerate(routes):
        r["ml_score"] = float(scores[i])

    # Sort by ML score (highest first)
    routes.sort(key=lambda x: -x["ml_score"])

    # Filter: only WETH pairs with score > 0.5
    top_routes = [r for r in routes if r["f_has_weth"] and r["ml_score"] > 0.3]

    print(f"\n=== TOP ROUTES (ML score > 0.3, WETH pairs) ===")
    print(f"Total qualifying routes: {len(top_routes)}")

    # Show top 20
    for i, r in enumerate(top_routes[:20]):
        longtail = r["token1"] if r["token0"] == WETH else r["token0"]
        print(f"  #{i+1} score={r['ml_score']:.3f} | {r['dex_a']}->{r['dex_b']} | "
              f"token={longtail[:10]}... | comp={r['f_comp_score_a']}/{r['f_comp_score_b']} | "
              f"{'V3/V2' if r['f_v3_v2_mix'] else 'same'}")

    # Output for Rust bot
    output = []
    for r in top_routes[:5000]:  # Top 5000 routes
        longtail = r["token1"] if r["token0"] == WETH else r["token0"]
        output.append({
            "pool_a": r["pool_a"],
            "pool_b": r["pool_b"],
            "dex_a": r["dex_a"],
            "dex_b": r["dex_b"],
            "token_in": WETH,
            "token_out": longtail,
            "ml_score": round(r["ml_score"], 4),
            "competition": max(r["f_comp_score_a"], r["f_comp_score_b"]),
            "is_v3_v2_mix": r["f_v3_v2_mix"],
            "fee_delta": r["f_fee_delta"],
        })

    with open(OUTPUT_FILE, "w") as f:
        json.dump(output, f)
    print(f"\nSaved {len(output)} prioritized routes to {OUTPUT_FILE}")

    # Stats
    zero_comp = sum(1 for r in output if r["competition"] <= 0)
    low_comp = sum(1 for r in output if 0 < r["competition"] <= 3)
    mixed = sum(1 for r in output if r["is_v3_v2_mix"])
    print(f"  Zero competition: {zero_comp}")
    print(f"  Low competition (1-3): {low_comp}")
    print(f"  V3/V2 mix (best for arb): {mixed}")

    return output


def main():
    print("=" * 60)
    print("LONGTAIL ROUTE ML SCORER")
    print("=" * 60)

    t0 = time.time()

    # 1. Load pools
    pools = load_pools()

    # 2. Extract features
    routes, pair_pools, token_count = extract_features(pools)

    if not routes:
        print("No cross-DEX routes found!")
        return

    # 3. Generate training labels (heuristic-based for initial model)
    labels = generate_training_labels(routes)

    # 4. Train model
    scores = train_model(routes, labels)

    # 5. Prioritize and output
    output = prioritize_routes(routes, scores)

    elapsed = time.time() - t0
    print(f"\nCompleted in {elapsed:.1f}s")
    print(f"Routes ready for bot: {OUTPUT_FILE}")


if __name__ == "__main__":
    main()
