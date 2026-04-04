"""
Gas Price Predictor — Predicts optimal gas for Base L2
Base has very cheap and stable gas. Model tracks patterns by hour.
"""
import time
from collections import defaultdict
import numpy as np


class GasPredictor:
    def __init__(self):
        self.hourly_gas = defaultdict(list)  # hour -> [gas_prices]
        self.last_gas = 100000  # 0.0001 gwei default for Base

    def record(self, gas_price):
        hour = time.gmtime().tm_hour
        self.hourly_gas[hour].append(gas_price)
        # Keep last 100 per hour
        if len(self.hourly_gas[hour]) > 100:
            self.hourly_gas[hour] = self.hourly_gas[hour][-100:]
        self.last_gas = gas_price

    def predict_optimal(self):
        """Return optimal gas price: median of current hour + 10% buffer"""
        hour = time.gmtime().tm_hour
        if hour in self.hourly_gas and len(self.hourly_gas[hour]) >= 5:
            median = int(np.median(self.hourly_gas[hour]))
            return int(median * 1.1)  # 10% above median
        return max(self.last_gas, 100000)  # fallback

    def predict_max(self):
        """Max gas we're willing to pay (for competitive arbs)"""
        return self.predict_optimal() * 3

    def is_gas_cheap(self):
        """Is gas currently below average? Good time for speculative trades."""
        optimal = self.predict_optimal()
        return self.last_gas <= optimal
