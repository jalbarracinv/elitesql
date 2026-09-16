"""Cost of one operation of the shop mix, engine against engine.

The sweep measures throughput and latency under concurrency; this measures
what a single operation costs with nothing else running, which is where the
per-row and per-statement work shows up undiluted. Both engines are seeded
from scratch in the same run, so the ratio does not depend on the machine.

The database is checkpointed before measuring: rows live in a published run,
not in the resident overlay. Reaching a published row costs about twice what
reaching a resident one costs, and a long-lived database is always in the
first state.

    python3 examples/saas_simulation/ops_cost.py
"""

import sys, time, random, os, subprocess
HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
sys.path.insert(0, os.path.join(HERE, "..", "..", "bindings", "python"))
TMP = os.environ.get("TMPDIR", "/tmp").rstrip("/")
# The mix is measured at whatever scale the caller asks for: the cost of a
# keyed row grows with the table on one engine and with the B-tree depth on
# the other, so the ratio between them is not a constant.
PRODUCTS = int(os.environ.get("OPS_COST_PRODUCTS", "5000"))
USERS = int(os.environ.get("OPS_COST_USERS", "20000"))
from saas import service, schema, drivers
for f in (f"{TMP}/ops_cost.esql", f"{TMP}/ops_cost.sqlite", f"{TMP}/ops_cost.sqlite-wal", f"{TMP}/ops_cost.sqlite-shm"):
    subprocess.run(["rm", "-rf", f])
def build(kind):
    c = drivers.open_embedded(f"{TMP}/ops_cost.esql", "balanced") if kind == "elite" else drivers.SqliteConnection(f"{TMP}/ops_cost.sqlite")
    service.create_schema(c, sqlite=(kind != "elite"))
    service.seed(c, users=USERS, products=PRODUCTS, sqlite=(kind != "elite"))
    # The canonical figures are measured against a published database: rows in
    # a run, not in the resident overlay. Reaching a published row costs about
    # twice what reaching a resident one costs, so a benchmark that skips this
    # measures a state no long-lived database is ever in.
    c.checkpoint()
    o = service.login(c, schema.user_email(3), schema.user_password(3))
    return c, o.data["user_id"], o.data["token"]
conns = {k: build(k) for k in ("elite", "sqlite")}
for c, uid, _ in conns.values():
    for pid in (11, 22, 33): service.add_to_cart(c, uid, pid, 1)
    service.checkout(c, uid)
    for pid in (44, 55, 66, 77, 88): service.add_to_cart(c, uid, pid, 1)
ops = {
    "browse":          lambda c,u,t,r: service.browse(c, r.choice(schema.CATEGORIES), r.randrange(3)),
    "product_detail":  lambda c,u,t,r: service.product_detail(c, r.randint(1,5000)),
    "session_check":   lambda c,u,t,r: service.session_check(c, t),
    # Keep the cart at a realistic size: the benchmark's own repetition would
    # otherwise grow it to hundreds of lines and make `view_cart` measure a
    # cart no simulated user ever has.
    "add_to_cart":     lambda c,u,t,r: (service.add_to_cart(c, u, r.randint(1,5000), 1),
                                        service.update_cart_item(c, u, r.randint(1,5000), 0)),
    "search_text":     lambda c,u,t,r: service.search_text(c, "bamboo kettle"),
    "view_cart":       lambda c,u,t,r: service.view_cart(c, u),
    "recommend":       lambda c,u,t,r: service.recommend(c, r.randint(1,5000)),
    "order_history":   lambda c,u,t,r: service.order_history(c, u),
    "write_review":    lambda c,u,t,r: service.write_review(c, u, r.randint(1,5000), 4, "ok"),
    "update_profile":  lambda c,u,t,r: service.update_profile(c, u, "n"),
    "admin_dashboard": lambda c,u,t,r: service.admin_dashboard(c, 0),
}
weights = {"browse":20,"product_detail":17,"session_check":12,"add_to_cart":10,"search_text":8,
           "view_cart":8,"recommend":7,"order_history":3,"write_review":2,"update_profile":1,"admin_dashboard":1}
print(f"escala: {USERS} cuentas, {PRODUCTS} productos")
print(f"{'operation':18s} {'weight':>6s} {'EliteSQL':>10s} {'SQLite':>10s} {'ratio':>7s} {'elite share':>12s}")
totals = {"elite":0.0, "sqlite":0.0}
for name, fn in ops.items():
    t = {}
    for kind, (c, uid, tok) in conns.items():
        # A realistic cart for the operations that read it: the benchmark's own
        # repetition of add_to_cart would otherwise leave hundreds of lines.
        cart = c.execute("SELECT id FROM carts WHERE user_id = ? AND status = 'open' LIMIT 1", [uid]).one
        if cart:
            c.execute("DELETE FROM cart_items WHERE cart_id = ?", [cart[0]])
        for pid in (44, 55, 66, 77, 88):
            service.add_to_cart(c, uid, pid, 1)
        r = random.Random(7); n = 60 if name == "admin_dashboard" else 400
        fn(c, uid, tok, random.Random(7))
        t0 = time.perf_counter()
        for _ in range(n): fn(c, uid, tok, r)
        t[kind] = (time.perf_counter()-t0)/n*1e6
    w = weights[name]/100
    totals["elite"] += t["elite"]*w; totals["sqlite"] += t["sqlite"]*w
    print(f"{name:18s} {weights[name]:5d}% {t['elite']:9.1f}us {t['sqlite']:9.1f}us {t['elite']/t['sqlite']:6.1f}x {t['elite']*w:10.1f}us")
print(f"\n{'weighted operation':18s} {'':6s} {totals['elite']:9.1f}us {totals['sqlite']:9.1f}us {totals['elite']/totals['sqlite']:6.1f}x")
for c,_,_ in conns.values(): c.close()
