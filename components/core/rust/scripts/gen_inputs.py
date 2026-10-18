"""Generates the deterministic, script-generated fixture inputs of the Rust clp-s reader corpus.

Usage: python3 gen_inputs.py <inputs-dir>

The outputs are committed; rerunning this script reproduces them byte for byte.
"""

import json
import random
import sys
from pathlib import Path

SEED = 20261017


def write_jsonl(path, records):
    with open(path, "w", encoding="utf-8") as f:
        for record in records:
            f.write(json.dumps(record, ensure_ascii=False, separators=(", ", ": ")))
            f.write("\n")


def many_schemas(rng):
    """240 records over ~120 schemas: combinatorial optional keys, type flips, and nesting."""
    records = []
    optional = ["alpha", "beta", "gamma", "delta", "eps"]
    for i in range(240):
        record = {"id": i}
        mask = i % 32
        for bit, key in enumerate(optional):
            if mask & (1 << bit):
                kind = (i // 32 + bit) % 4
                if kind == 0:
                    record[key] = i * 10 + bit
                elif kind == 1:
                    record[key] = f"v{i}_{bit}"
                elif kind == 2:
                    record[key] = f"text value {i} {bit}"
                else:
                    record[key] = {"inner": i, "f": round(i / 7, 3)}
        if i % 5 == 0:
            record["arr"] = [i, f"s{i}", {"k": i}]
        if i % 11 == 0:
            record["flag"] = i % 22 == 0
        if i % 13 == 0:
            record["nested"] = {"lvl": {f"k{i % 4}": rng.randint(0, 1000)}}
        record["msg"] = f"event {i} from worker {rng.randint(1, 8)}"
        records.append(record)
    return records


def long_strings(rng):
    """Records with very long VarStrings, ClpStrings, array strings and keys."""
    alphabet = "abcdefghijklmnopqrstuvwxyz0123456789"
    records = []
    var_long = "".join(rng.choice(alphabet) for _ in range(70000))
    records.append({"id": 0, "var": var_long})
    words = ["".join(rng.choice(alphabet) for _ in range(rng.randint(1, 12))) for _ in range(12000)]
    records.append({"id": 1, "clp": " ".join(words)})
    clp_with_vars = " ".join(
        f"id={rng.randint(0, 10**9)} val={rng.random():.6f} hex=0x{rng.randint(0, 2**32):x}"
        for _ in range(2000)
    )
    records.append({"id": 2, "clp": clp_with_vars})
    records.append({"id": 3, "arr": [var_long[:20000], " ".join(words[:2000])]})
    records.append({"id": 4, "k" * 4096: "long key", "var": "short"})
    records.append({"id": 5, "clp": "short clp string", "var": var_long[:100]})
    unicode_long = "".join(rng.choice("中文日本語한국어éèêëçàù😀🎉") for _ in range(20000))
    records.append(
        {
            "id": 6,
            "uni": unicode_long,
            "uni_clp": unicode_long[:5000] + " " + unicode_long[5000:10000],
        }
    )
    return records


def logs(rng):
    """3000 log-like records with an epoch-ms `ts`, used for packed-stream and split coverage."""
    levels = ["INFO"] * 6 + ["WARN"] * 2 + ["ERROR", "DEBUG"]
    services = ["auth", "billing", "search", "ingest", "query"]
    templates = [
        "user {uid} logged in from 10.0.{a}.{b}",
        "request {rid} failed with status {status}",
        "query on table t{t} returned {rows} rows in {ms}ms",
        "cache hit ratio {ratio}",
        "job job-{job:08x} finished",
    ]
    records = []
    ts = 1735689600000
    for i in range(3000):
        ts += rng.randint(1, 1500)
        template = templates[i % len(templates)]
        msg = template.format(
            uid=rng.randint(1, 500),
            a=rng.randint(0, 255),
            b=rng.randint(0, 255),
            rid=rng.randint(1, 100000),
            status=rng.choice([400, 403, 404, 500, 503]),
            t=rng.randint(1, 20),
            rows=rng.randint(0, 10000),
            ms=rng.randint(1, 9000),
            ratio=f"{rng.random():.3f}",
            job=rng.randint(0, 2**32 - 1),
        )
        record = {
            "ts": ts,
            "seq": i,
            "level": rng.choice(levels),
            "service": rng.choice(services),
            "latency_ms": rng.randint(0, 10000),
            "msg": msg,
        }
        if i % 7 == 0:
            record["ctx"] = {"trace": f"t{rng.randint(0, 10**6):06d}", "span": rng.randint(0, 99)}
        if i % 9 == 0:
            record["tags"] = rng.sample(["a", "b", "c", "d", "e"], rng.randint(0, 3))
        if i % 17 == 0:
            record["score"] = round(rng.uniform(-100, 100), 2)
        records.append(record)
    return records


def main():
    out_dir = Path(sys.argv[1])
    out_dir.mkdir(parents=True, exist_ok=True)
    write_jsonl(out_dir / "many-schemas.jsonl", many_schemas(random.Random(SEED)))
    write_jsonl(out_dir / "long-strings.jsonl", long_strings(random.Random(SEED + 1)))
    write_jsonl(out_dir / "logs.jsonl", logs(random.Random(SEED + 2)))


if __name__ == "__main__":
    main()
