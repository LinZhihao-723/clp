"""Synthetic JSONL generator for the `synthetic` benchmark dataset.

Produces records with nested objects, unstructured arrays (scalars and objects), polymorphic
fields (the same key holding int / float / string / bool / null / object / array), optional
subtrees, and a combinatorial `metrics` object so the archive ends up with many schemas (ERTs).

Usage: python3 -I gen_synthetic.py OUT_PATH NUM_RECORDS
"""

import json
import random
import sys

SEED = 20261017
SERVICES = [f"svc-{name}" for name in (
    "auth", "billing", "cart", "catalog", "checkout", "email", "gateway", "inventory", "ledger",
    "media", "notify", "orders", "payments", "profile", "quota", "ratings", "search", "shipping",
    "tax", "users",
)]
LEVELS = ["DEBUG", "INFO", "INFO", "INFO", "WARN", "ERROR"]
REGIONS = ["us-east-1", "us-west-2", "eu-west-1", "ap-south-1", "sa-east-1"]
TAG_POOL = [f"tag{i}" for i in range(40)]
METRIC_KEYS = [f"m{i:02d}" for i in range(6)]
TEMPLATES = [
    "User {user} logged in from {ip} after {ms} ms",
    "Request {req} to {svc} failed with status {status}",
    "Cache miss for key {key} in region {region}",
    "Processed batch {batch} with {n} items in {sec} s",
    "Connection pool {pool} exhausted, waiting {ms} ms",
    "Retrying job {req} attempt {n} of 5",
    "GC pause {ms} ms heap {n} MB",
]
NEEDLE = "needle-7f3a9c1e"


def rand_hex(rng, n):
    return "".join(rng.choice("0123456789abcdef") for _ in range(n))


def make_message(rng, i):
    template = rng.choice(TEMPLATES)
    msg = template.format(
        user=f"user{rng.randrange(5000)}",
        ip=f"10.{rng.randrange(256)}.{rng.randrange(256)}.{rng.randrange(256)}",
        ms=rng.randrange(1, 20000),
        req=f"req-{rand_hex(rng, 8)}",
        svc=rng.choice(SERVICES),
        status=rng.choice([400, 401, 403, 404, 429, 500, 502, 503]),
        key=f"k:{rand_hex(rng, 6)}",
        region=rng.choice(REGIONS),
        batch=rng.randrange(1_000_000),
        n=rng.randrange(1, 2000),
        sec=f"{rng.random() * 100:.3f}",
        pool=f"pool-{rng.randrange(16)}",
    )
    if i % 100_003 == 777:
        msg += f" trace {NEEDLE}"
    return msg


def make_payload(rng):
    kind = rng.randrange(8)
    if kind == 0:
        return rng.randrange(-1000, 100_000)
    if kind == 1:
        return round(rng.uniform(-50.0, 5000.0), 4)
    if kind == 2:
        return f"p-{rand_hex(rng, 6)}"
    if kind == 3:
        return rng.random() < 0.5
    if kind == 4:
        return None
    if kind == 5:
        return {"code": rng.randrange(100, 600), "detail": f"d{rng.randrange(50)}"}
    if kind == 6:
        return [rng.randrange(1000) for _ in range(rng.randrange(1, 5))]
    return f"payload text with spaces {rng.randrange(1000)}"


def make_record(rng, i, base_ms):
    ts_ms = base_ms + i * 7 + rng.randrange(7)
    secs, ms = divmod(ts_ms, 1000)
    minutes, s = divmod(secs, 60)
    hours, m = divmod(minutes, 60)
    days, h = divmod(hours, 24)
    record = {
        "ts": f"2026-01-{1 + days:02d}T{h:02d}:{m:02d}:{s:02d}.{ms:03d}Z",
        "level": rng.choice(LEVELS),
        "service": rng.choice(SERVICES),
        "msg": make_message(rng, i),
        "req": {
            "id": f"{rand_hex(rng, 8)}-{rand_hex(rng, 4)}",
            "user": {"id": rng.randrange(100_000), "name": f"user{rng.randrange(5000)}"},
        },
        "payload": make_payload(rng),
    }
    if rng.random() < 0.6:
        record["req"]["user"]["tags"] = rng.sample(TAG_POOL, rng.randrange(1, 4))
    if rng.random() < 0.5:
        record["req"]["geo"] = {
            "lat": round(rng.uniform(-90, 90), 5),
            "lon": round(rng.uniform(-180, 180), 5),
        }
    if rng.random() < 0.4:
        record["attrs"] = [
            {"k": rng.choice(["region", "zone", "tier", "shard"]),
             "v": rng.choice([rng.randrange(100), rng.choice(REGIONS)])}
            for _ in range(rng.randrange(1, 4))
        ]
    metric_keys = rng.sample(METRIC_KEYS, rng.randrange(1, 4))
    record["metrics"] = {
        k: (round(rng.random() * 100, 3) if k == "m05" and rng.random() < 0.5 else rng.randrange(1000))
        for k in metric_keys
    }
    if rng.random() < 0.05:
        record["err"] = {
            "code": rng.choice([500, 502, 503, 504]),
            "stack": [f"frame{j} at mod{rng.randrange(30)}.rs:{rng.randrange(900)}"
                      for j in range(rng.randrange(1, 4))],
        }
    return record


def main():
    out_path, num_records = sys.argv[1], int(sys.argv[2])
    rng = random.Random(SEED)
    base_ms = 0
    with open(out_path, "w", encoding="utf-8") as out:
        for i in range(num_records):
            out.write(json.dumps(make_record(rng, i, base_ms), separators=(",", ":")))
            out.write("\n")


if __name__ == "__main__":
    main()
