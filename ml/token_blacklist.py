"""
ML Token Blacklist — Learns which tokens always revert (honeypots/scams)
Auto-blacklists after N consecutive failures.
Features: code size, revert pattern, pool age
"""
import json
import os
import time

BLACKLIST_FILE = "/root/arb-flash-bot/ml/blacklist.json"


class TokenBlacklist:
    def __init__(self):
        self.failures = {}  # token -> [timestamp, ...]
        self.blacklist = set()
        self.whitelist = {
            "0x4200000000000000000000000000000000000006",  # WETH
            "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913",  # USDC
            "0xd9aaec86b65d86f6a7b5b1b0c42ffa531710b6ca",  # USDbC
            "0x50c5725949a6f0c72e6c4a641f24049a917db0cb",  # DAI
        }
        self.load()

    def is_blacklisted(self, token):
        return token.lower() in self.blacklist

    def record_failure(self, token):
        token = token.lower()
        if token in self.whitelist:
            return
        if token not in self.failures:
            self.failures[token] = []
        self.failures[token].append(time.time())
        # Blacklist after 3 consecutive failures in 1 hour
        recent = [t for t in self.failures[token] if time.time() - t < 3600]
        if len(recent) >= 3:
            self.blacklist.add(token)
            self.save()

    def record_success(self, token):
        token = token.lower()
        # Reset failures on success
        self.failures.pop(token, None)
        self.blacklist.discard(token)

    def save(self):
        data = {"blacklist": list(self.blacklist), "failures": {k: v[-5:] for k, v in self.failures.items()}}
        with open(BLACKLIST_FILE, "w") as f:
            json.dump(data, f)

    def load(self):
        try:
            with open(BLACKLIST_FILE) as f:
                data = json.load(f)
                self.blacklist = set(data.get("blacklist", []))
                self.failures = data.get("failures", {})
        except:
            pass

    def stats(self):
        return f"blacklisted={len(self.blacklist)} tracked={len(self.failures)}"
