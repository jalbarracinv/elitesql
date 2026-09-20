"""Cost of one mini-SaaS operation, engine against engine.

The sweep measures concurrency; this benchmark measures an operation without
other requests. It seeds each selected engine from scratch, checkpoints before
timing and records the exact request mix it used.

``full-v2`` (the default) measures the sixteen operations used by
``saas.vuser`` and normalizes their weights. ``historical-v1`` keeps the
old eleven-operation weights-over-100 formula only for comparison with old
reports; it is a partial mix and is labelled as such.
"""

from __future__ import annotations

import argparse
import json
import os
import random
import statistics
import sys
import tempfile
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
sys.path.insert(0, str(HERE / ".." / ".." / "bindings" / "python"))

from saas import drivers, schema, service

FULL_METRIC = "full-v2"
HISTORICAL_METRIC = "historical-v1"


@dataclass
class Sample:
    user_id: int
    email: str
    token: str
    product_id: int


@dataclass
class Operation:
    call: Callable[[Any, Sample, int], service.Outcome]
    prepare: Callable[[Any, list[Sample], int], None]
    heavy: bool = False


def no_prepare(_conn: Any, _samples: list[Sample], _products: int) -> None:
    pass


def product_ids(products: int, count: int, seed: int) -> list[int]:
    rng = random.Random(seed)
    return [rng.randint(1, products) for _ in range(count)]


def make_samples(conn: Any, count: int, products: int, tag: str) -> list[Sample]:
    """Prepare independent identities outside the timed interval."""
    samples = []
    for position, product_id in enumerate(product_ids(products, count, 10_000 + len(tag))):
        email = f"ops-cost-{tag}-{position}@example.com"
        password = "ops-cost-password"
        signup = service.signup(conn, email, password, f"Ops cost {tag} {position}")
        if signup.business != "ok":
            raise RuntimeError(f"could not prepare {tag} sample {position}: {signup.business}")
        login = service.login(conn, email, password)
        if login.business != "ok":
            raise RuntimeError(f"could not log in {tag} sample {position}: {login.business}")
        samples.append(Sample(login.data["user_id"], email, login.data["token"], product_id))
    return samples


def fill_cart(conn: Any, sample: Sample, products: int) -> None:
    for product_id in product_ids(products, 5, sample.user_id):
        outcome = service.add_to_cart(conn, sample.user_id, product_id, 1)
        if outcome.business != "ok":
            raise RuntimeError(f"could not prepare cart: {outcome.business}")


def prepare_carts(conn: Any, samples: list[Sample], products: int) -> None:
    for sample in samples:
        fill_cart(conn, sample, products)


def prepare_cart_items(conn: Any, samples: list[Sample], _products: int) -> None:
    for sample in samples:
        outcome = service.add_to_cart(conn, sample.user_id, sample.product_id, 2)
        if outcome.business != "ok":
            raise RuntimeError(f"could not prepare cart item: {outcome.business}")


def prepare_orders(conn: Any, samples: list[Sample], products: int) -> None:
    for sample in samples:
        fill_cart(conn, sample, products)
        outcome = service.checkout(conn, sample.user_id)
        if outcome.business != "ok":
            raise RuntimeError(f"could not prepare order history: {outcome.business}")


def relogin(conn: Any, sample: Sample, _position: int) -> service.Outcome:
    service.logout(conn, sample.token)
    outcome = service.login(conn, sample.email, "ops-cost-password")
    if outcome.business == "ok":
        sample.token = outcome.data["token"]
    return outcome


def operation_definitions() -> dict[str, Operation]:
    return {
        "browse": Operation(lambda conn, _sample, n: service.browse(conn, schema.CATEGORIES[n % len(schema.CATEGORIES)], n % 5), no_prepare),
        "product_detail": Operation(lambda conn, sample, _n: service.product_detail(conn, sample.product_id), no_prepare),
        "session_check": Operation(lambda conn, sample, _n: service.session_check(conn, sample.token), no_prepare),
        "add_to_cart": Operation(lambda conn, sample, _n: service.add_to_cart(conn, sample.user_id, sample.product_id, 1), no_prepare),
        "search_text": Operation(lambda conn, _sample, _n: service.search_text(conn, "bamboo kettle"), no_prepare),
        "view_cart": Operation(lambda conn, sample, _n: service.view_cart(conn, sample.user_id), prepare_carts),
        "recommend": Operation(lambda conn, sample, _n: service.recommend(conn, sample.product_id), no_prepare),
        "checkout": Operation(lambda conn, sample, _n: service.checkout(conn, sample.user_id), prepare_carts, heavy=True),
        "update_cart_item": Operation(lambda conn, sample, _n: service.update_cart_item(conn, sample.user_id, sample.product_id, 1), prepare_cart_items),
        "order_history": Operation(lambda conn, sample, _n: service.order_history(conn, sample.user_id), prepare_orders),
        "write_review": Operation(lambda conn, sample, _n: service.write_review(conn, sample.user_id, sample.product_id, 4, "ops cost review"), no_prepare),
        "relogin": Operation(relogin, no_prepare),
        "update_profile": Operation(lambda conn, sample, n: service.update_profile(conn, sample.user_id, f"Ops cost {n}"), no_prepare),
        "admin_dashboard": Operation(lambda conn, _sample, _n: service.admin_dashboard(conn, 0), no_prepare, heavy=True),
        "signup": Operation(lambda conn, sample, n: service.signup(conn, f"ops-cost-new-{sample.user_id}-{n}@example.com", "pw", "New"), no_prepare),
        "restock": Operation(lambda conn, sample, _n: service.restock(conn, [sample.product_id], 1), no_prepare),
    }


def build(kind: str, root: Path, users: int, products: int, scenario: str):
    path = root / ("elite.esql" if kind == "elite" else "sqlite.db")
    conn = drivers.open_embedded(str(path), "balanced") if kind == "elite" else drivers.SqliteConnection(str(path))
    service.create_schema(conn, sqlite=(kind == "sqlite"))
    service.seed(conn, users=users, products=products, sqlite=(kind == "sqlite"))
    if scenario == "compound-index":
        # Keep every baseline index and add only the browse access path. The
        # same portable DDL reaches SQLite so the scenario remains comparable.
        ddl = "CREATE INDEX ON products (category, price_cents)"
        conn.execute(drivers.sqlite_ddl(ddl) if kind == "sqlite" else ddl)
    conn.checkpoint()
    return conn


def benchmark_operation(conn: Any, name: str, operation: Operation, products: int, iterations: int) -> float:
    samples = make_samples(conn, iterations, products, name)
    operation.prepare(conn, samples, products)
    # Every timed write targets a distinct prepared account. This avoids the
    # old benchmark's cart growth and makes all configured product scales real.
    started = time.perf_counter()
    for position, sample in enumerate(samples):
        outcome = operation.call(conn, sample, position)
        if outcome.business != "ok":
            raise RuntimeError(f"{name} sample {position} returned {outcome.business}")
    return (time.perf_counter() - started) * 1e6 / iterations


def metric_definition(metric: str) -> tuple[dict[str, float], float, str]:
    if metric == FULL_METRIC:
        weights = dict(schema.OPERATION_WEIGHTS)
        return weights, sum(weights.values()), "normalized full virtual-user mix"
    weights = dict(schema.HISTORICAL_OPS_COST_WEIGHTS)
    return weights, 100.0, "historical partial mix; weights intentionally divide by 100"


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--metric", choices=(FULL_METRIC, HISTORICAL_METRIC), default=FULL_METRIC)
    parser.add_argument("--products", type=int, default=int(os.environ.get("OPS_COST_PRODUCTS", "5000")))
    parser.add_argument("--users", type=int, default=int(os.environ.get("OPS_COST_USERS", "20000")))
    parser.add_argument("--iterations", type=int, default=400)
    parser.add_argument("--heavy-iterations", type=int, default=60)
    parser.add_argument("--repetitions", type=int, default=3)
    parser.add_argument("--engines", default="elite,sqlite", help="comma-separated subset of elite,sqlite")
    parser.add_argument("--scenario", choices=("baseline", "compound-index"), default="baseline")
    parser.add_argument("--out", type=Path, help="write raw timings and configuration as JSON")
    args = parser.parse_args()
    if min(args.products, args.users, args.iterations, args.heavy_iterations, args.repetitions) < 1:
        parser.error("products, users, iterations, heavy-iterations and repetitions must be positive")
    engines = [engine.strip() for engine in args.engines.split(",") if engine.strip()]
    if not engines or any(engine not in {"elite", "sqlite"} for engine in engines):
        parser.error("--engines must be a non-empty subset of elite,sqlite")

    weights, divisor, description = metric_definition(args.metric)
    definitions = operation_definitions()
    missing = set(weights).difference(definitions)
    if missing:
        raise RuntimeError(f"metric refers to missing operations: {sorted(missing)}")
    print(f"metric: {args.metric} ({description})")
    print(f"weights: {sum(weights.values()):.1f}; divisor: {divisor:.1f}; operations: {len(weights)}")
    print(f"scenario: {args.scenario}; scale: {args.users} accounts, {args.products} products; samples: {args.iterations} ({args.heavy_iterations} heavy); repetitions: {args.repetitions}")

    raw: dict[str, dict[str, list[float]]] = {name: {engine: [] for engine in engines} for name in weights}
    for repetition in range(args.repetitions):
        # Alternating engine order limits systematic thermal/cache bias.
        shift = repetition % len(engines)
        order = engines[shift:] + engines[:shift]
        with tempfile.TemporaryDirectory(prefix=f"elitesql-ops-cost-r{repetition}-") as temp:
            root = Path(temp)
            conns = {}
            try:
                for engine in order:
                    conns[engine] = build(engine, root, args.users, args.products, args.scenario)
                    for name in weights:
                        definition = definitions[name]
                        count = args.heavy_iterations if definition.heavy else args.iterations
                        raw[name][engine].append(benchmark_operation(conns[engine], name, definition, args.products, count))
            finally:
                for conn in conns.values():
                    conn.close()

    medians = {name: {engine: statistics.median(samples) for engine, samples in values.items()} for name, values in raw.items()}
    totals = {engine: sum(medians[name][engine] * weight / divisor for name, weight in weights.items()) for engine in engines}
    print(f"{'operation':18s} {'weight':>7s}" + "".join(f" {engine:>20s}" for engine in engines))
    for name, weight in weights.items():
        columns = []
        for engine in engines:
            samples = raw[name][engine]
            columns.append(f"{medians[name][engine]:8.1f} us [{min(samples):.1f},{max(samples):.1f}]")
        print(f"{name:18s} {weight:6.1f}%" + "".join(f" {column:>20s}" for column in columns))
    print("weighted operation (median per operation): " + ", ".join(f"{engine}={total:.1f} us" for engine, total in totals.items()))
    if len(engines) == 2:
        print(f"ratio elite/sqlite: {totals['elite'] / totals['sqlite']:.2f}x")
    if args.out:
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_text(json.dumps({"metric": args.metric, "description": description, "scenario": args.scenario, "weights": weights, "divisor": divisor, "products": args.products, "users": args.users, "iterations": args.iterations, "heavy_iterations": args.heavy_iterations, "repetitions": args.repetitions, "raw_us": raw, "median_us": medians, "weighted_us": totals}, indent=2) + "\n")


if __name__ == "__main__":
    main()
