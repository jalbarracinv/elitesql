"""Business operations of the mini-SaaS.

Every public function takes a connection (see ``drivers.py``) and plain
arguments, runs one logical request of the application and returns a small
dict. Multi-statement writes run inside ``run_transaction`` which retries on
optimistic-commit conflicts and reports how many retries it needed through
``Outcome.retries``. Operations never print and never sleep.
"""

from __future__ import annotations

import random
import secrets
import time
from dataclasses import dataclass, field
from typing import Any, Callable

from . import schema
from .drivers import Conflict, NotSupported, Result

PAGE_SIZE = 20
LOYALTY_POINTS_PER_ORDER = 10
MAX_RETRIES = 8
# First backoff window before a conflicting unit of work is retried; it
# doubles per attempt up to 64x, and the actual pause is uniform inside it.
RETRY_BACKOFF_BASE = 0.0005


@dataclass
class Outcome:
    """What one operation produced, besides its latency."""
    data: Any = None
    retries: int = 0          # optimistic-commit conflicts retried
    business: str = "ok"      # ok | out_of_stock | empty_cart | bad_password | not_found
    rows_read: int = 0
    extra: dict = field(default_factory=dict)


def now_ms() -> int:
    return int(time.time() * 1000)


class Abort(Exception):
    """Raised inside a unit of work to roll back and return ``value``."""

    def __init__(self, value: Any):
        super().__init__("transaction aborted by the application")
        self.value = value


def run_transaction(conn, unit: Callable[[Any], Any], retries: int = MAX_RETRIES):
    """Run ``unit(tx)`` and commit; retry the whole unit on ``Conflict``.

    A unit that raises ``Abort(value)`` is rolled back and ``value`` is
    returned, so a business refusal never publishes a partial write.
    """
    attempt = 0
    while True:
        tx = conn.transaction()
        try:
            value = unit(tx)
            tx.commit()
            return value, attempt
        except Abort as abort:
            tx.rollback()
            return abort.value, attempt
        except Conflict:
            tx.rollback()
            attempt += 1
            if attempt > retries:
                raise
            # Back off before colliding again. Without this, every loser of a
            # contended commit retries at once and collides with the same
            # peers: the engine's own autocommit retry uses the same
            # exponential backoff with jitter for that reason.
            time.sleep(random.uniform(0.0, RETRY_BACKOFF_BASE * (2 ** min(attempt - 1, 6))))
        except Exception:
            tx.rollback()
            raise


# ------------------------------------------------------------ schema/seed --


def create_schema(conn, sqlite: bool = False) -> None:
    from .drivers import sqlite_ddl
    for _, ddl in schema.TABLES:
        conn.execute(sqlite_ddl(ddl) if sqlite else ddl)
    for ddl in schema.INDEXES:
        conn.execute(sqlite_ddl(ddl) if sqlite else ddl)


def seed(conn, users: int, products: int, sqlite: bool = False, batch: int = 2000) -> dict:
    """Deterministic seed. Returns counts so the runner can log them."""
    t0 = time.perf_counter()
    rows = list(schema.generate_products(products))
    cols = "sku, name, description, category, price_cents, stock, sold, embedding"
    ph = "?, ?, ?, ?, ?, ?, 0, ?"
    if sqlite:
        cols = cols.replace(", embedding", "")
        ph = "?, ?, ?, ?, ?, ?, 0"
    for start in range(0, len(rows), batch):
        def unit(tx, chunk=rows[start:start + batch]):
            for p in chunk:
                params = [p["sku"], p["name"], p["description"], p["category"],
                          p["price_cents"], p["stock"]]
                if not sqlite:
                    params.append(p["embedding"])
                tx.execute(f"INSERT INTO products ({cols}) VALUES ({ph})", params)
        run_transaction(conn, unit)
    for start in range(0, users, batch):
        def unit(tx, lo=start, hi=min(users, start + batch)):
            for i in range(lo, hi):
                email = schema.user_email(i)
                tx.execute(
                    "INSERT INTO users (email, password_hash, display_name, plan, credits) "
                    "VALUES (?, ?, ?, ?, 0)",
                    [email, schema.password_hash(email, schema.user_password(i)),
                     f"User {i}", "pro" if i % 5 == 0 else "free"],
                )
        run_transaction(conn, unit)
    conn.create_text_index("products", "description")
    if conn.supports_vectors:
        conn.create_vector_index("products", "embedding")
    return {"users": users, "products": products,
            "seed_seconds": round(time.perf_counter() - t0, 2)}


# ---------------------------------------------------------------- account --


def signup(conn, email: str, password: str, display_name: str) -> Outcome:
    def unit(tx):
        user = tx.execute(
            "INSERT INTO users (email, password_hash, display_name) VALUES (?, ?, ?)",
            [email, schema.password_hash(email, password), display_name],
        ).lastrowid
        tx.execute("INSERT INTO carts (user_id, status, updated_ms) VALUES (?, 'open', ?)",
                   [user, now_ms()])
        tx.execute("INSERT INTO audit_log (user_id, action, at_ms) VALUES (?, 'signup', ?)",
                   [user, now_ms()])
        return user
    user_id, retries = run_transaction(conn, unit)
    return Outcome(data={"user_id": user_id}, retries=retries)


def login(conn, email: str, password: str) -> Outcome:
    row = conn.execute(
        "SELECT id, password_hash FROM users WHERE email = ? LIMIT 1", [email]
    ).one
    if row is None:
        return Outcome(business="not_found")
    user_id, stored = row
    if stored != schema.password_hash(email, password):
        return Outcome(business="bad_password")
    token = secrets.token_hex(16)

    def unit(tx):
        tx.execute("INSERT INTO sessions (token, user_id, last_seen_ms) VALUES (?, ?, ?)",
                   [token, user_id, now_ms()])
        tx.execute("UPDATE users SET login_count = login_count + 1 WHERE id = ?", [user_id])
        tx.execute("INSERT INTO audit_log (user_id, action, at_ms) VALUES (?, 'login', ?)",
                   [user_id, now_ms()])
    _, retries = run_transaction(conn, unit)
    return Outcome(data={"user_id": user_id, "token": token}, retries=retries)


def session_check(conn, token: str) -> Outcome:
    """The auth middleware: token -> user, plus a last-seen touch."""
    row = conn.execute(
        "SELECT s.user_id, u.display_name, u.plan, u.credits FROM sessions s "
        "JOIN users u ON u.id = s.user_id WHERE s.token = ? LIMIT 1", [token]
    ).one
    if row is None:
        return Outcome(business="not_found")
    conn.execute("UPDATE sessions SET last_seen_ms = ? WHERE token = ?", [now_ms(), token])
    return Outcome(data={"user_id": row[0], "plan": row[2]}, rows_read=1)


def logout(conn, token: str) -> Outcome:
    affected = conn.execute("DELETE FROM sessions WHERE token = ?", [token]).affected
    return Outcome(business="ok" if affected else "not_found")


def update_profile(conn, user_id: int, display_name: str) -> Outcome:
    conn.execute("UPDATE users SET display_name = ? WHERE id = ?", [display_name, user_id])
    return Outcome()


# ---------------------------------------------------------------- catalog --


def browse(conn, category: str, page: int) -> Outcome:
    rows = conn.execute(
        "SELECT id, name, price_cents, stock FROM products WHERE category = ? "
        "ORDER BY price_cents ASC LIMIT ? OFFSET ?",
        [category, PAGE_SIZE, page * PAGE_SIZE],
    ).rows
    return Outcome(data=[r[0] for r in rows], rows_read=len(rows))


def search_text(conn, term: str) -> Outcome:
    hits = conn.search_text("products", "description", term, top_k=10)
    return Outcome(data=[h["id"] for h in hits], rows_read=len(hits))


def product_detail(conn, product_id: int) -> Outcome:
    product = conn.execute(
        "SELECT id, name, description, price_cents, stock, rating_sum, rating_count "
        "FROM products WHERE id = ? LIMIT 1", [product_id]
    ).one
    if product is None:
        return Outcome(business="not_found")
    reviews = conn.execute(
        "SELECT stars, body, created_ms FROM reviews WHERE product_id = ? "
        "ORDER BY created_ms DESC LIMIT 5", [product_id]
    ).rows
    rating = product[5] / product[6] if product[6] else None
    return Outcome(data={"price": product[3], "stock": product[4], "rating": rating},
                   rows_read=1 + len(reviews))


def recommend(conn, product_id: int) -> Outcome:
    """Nearest neighbours of a product by embedding; category fallback on SQLite."""
    if conn.supports_vectors:
        row = conn.execute("SELECT embedding, category FROM products WHERE id = ? LIMIT 1",
                           [product_id]).one
        if row is None:
            return Outcome(business="not_found")
        hits = conn.search_vector("products", "embedding", row[0], top_k=6)
        return Outcome(data=[h["id"] for h in hits if h["id"] != product_id], rows_read=len(hits))
    row = conn.execute("SELECT category FROM products WHERE id = ? LIMIT 1", [product_id]).one
    if row is None:
        return Outcome(business="not_found")
    rows = conn.execute(
        "SELECT id FROM products WHERE category = ? ORDER BY sold DESC LIMIT 6", [row[0]]
    ).rows
    return Outcome(data=[r[0] for r in rows if r[0] != product_id], rows_read=len(rows),
                   extra={"fallback": True})


# ------------------------------------------------------------------- cart --


def _open_cart(tx, user_id: int) -> int:
    row = tx.execute("SELECT id FROM carts WHERE user_id = ? AND status = 'open' LIMIT 1",
                     [user_id]).one
    if row is not None:
        return row[0]
    return tx.execute("INSERT INTO carts (user_id, status, updated_ms) VALUES (?, 'open', ?)",
                      [user_id, now_ms()]).lastrowid


def add_to_cart(conn, user_id: int, product_id: int, qty: int) -> Outcome:
    def unit(tx):
        price = tx.execute("SELECT price_cents FROM products WHERE id = ? LIMIT 1",
                           [product_id]).scalar
        if price is None:
            return "not_found"
        cart_id = _open_cart(tx, user_id)
        item = tx.execute(
            "SELECT id FROM cart_items WHERE cart_id = ? AND product_id = ? LIMIT 1",
            [cart_id, product_id]).one
        if item is None:
            tx.execute("INSERT INTO cart_items (cart_id, product_id, qty, unit_price_cents) "
                       "VALUES (?, ?, ?, ?)", [cart_id, product_id, qty, price])
        else:
            tx.execute("UPDATE cart_items SET qty = qty + ? WHERE id = ?", [qty, item[0]])
        tx.execute("UPDATE carts SET updated_ms = ? WHERE id = ?", [now_ms(), cart_id])
        return "ok"
    business, retries = run_transaction(conn, unit)
    return Outcome(business=business, retries=retries)


# Rows `view_cart` returns at most (its LIMIT); a larger cart is shown truncated.
VIEW_CART_LIMIT = 100


def view_cart(conn, user_id: int) -> Outcome:
    cart = conn.execute("SELECT id FROM carts WHERE user_id = ? AND status = 'open' LIMIT 1",
                        [user_id]).one
    if cart is None:
        return Outcome(data={}, rows_read=0)
    rows = conn.execute(
        "SELECT ci.product_id, ci.qty, ci.unit_price_cents, p.name FROM cart_items ci "
        "JOIN products p ON p.id = ci.product_id WHERE ci.cart_id = ? ORDER BY ci.id ASC LIMIT 100",
        [cart[0]],
    ).rows
    return Outcome(data={r[0]: r[1] for r in rows}, rows_read=len(rows))


def update_cart_item(conn, user_id: int, product_id: int, qty: int) -> Outcome:
    def unit(tx):
        cart_id = _open_cart(tx, user_id)
        if qty <= 0:
            affected = tx.execute("DELETE FROM cart_items WHERE cart_id = ? AND product_id = ?",
                                  [cart_id, product_id]).affected
        else:
            affected = tx.execute(
                "UPDATE cart_items SET qty = ? WHERE cart_id = ? AND product_id = ?",
                [qty, cart_id, product_id]).affected
        tx.execute("UPDATE carts SET updated_ms = ? WHERE id = ?", [now_ms(), cart_id])
        return "ok" if affected else "not_found"
    business, retries = run_transaction(conn, unit)
    return Outcome(business=business, retries=retries)


def checkout(conn, user_id: int) -> Outcome:
    """Turn the open cart into an order, decrementing stock atomically.

    Stock is decremented with a guarded UPDATE (``stock >= qty``); a product
    that ran out fails the whole checkout with ``out_of_stock`` and nothing is
    published. On EliteSQL the decrement is a delta update: concurrent
    checkouts of the same hot product both commit while the guard holds on the
    newer version, and only a guard that fails there reruns the unit.
    """
    def unit(tx):
        cart_id = _open_cart(tx, user_id)
        items = tx.execute(
            "SELECT product_id, qty, unit_price_cents FROM cart_items WHERE cart_id = ? LIMIT 100",
            [cart_id]).rows
        if not items:
            raise Abort(("empty_cart", None))
        total = 0
        count = 0
        for product_id, qty, price in items:
            affected = tx.execute(
                "UPDATE products SET stock = stock - ?, sold = sold + ? WHERE id = ? AND stock >= ?",
                [qty, qty, product_id, qty]).affected
            if not affected:
                # Earlier lines already decremented stock inside this
                # transaction: rolling back is what keeps sold == order lines.
                raise Abort(("out_of_stock", product_id))
            total += qty * price
            count += qty
        order_id = tx.execute(
            "INSERT INTO orders (user_id, status, total_cents, item_count, created_ms) "
            "VALUES (?, 'paid', ?, ?, ?)", [user_id, total, count, now_ms()]).lastrowid
        for product_id, qty, price in items:
            tx.execute("INSERT INTO order_items (order_id, product_id, qty, unit_price_cents) "
                       "VALUES (?, ?, ?, ?)", [order_id, product_id, qty, price])
        tx.execute("DELETE FROM cart_items WHERE cart_id = ?", [cart_id])
        tx.execute("UPDATE carts SET updated_ms = ? WHERE id = ?", [now_ms(), cart_id])
        tx.execute("UPDATE users SET credits = credits + ? WHERE id = ?",
                   [LOYALTY_POINTS_PER_ORDER, user_id])
        tx.execute("INSERT INTO audit_log (user_id, action, detail, at_ms) VALUES (?, 'checkout', ?, ?)",
                   [user_id, str(order_id), now_ms()])
        return ("ok", order_id)
    (business, ref), retries = run_transaction(conn, unit)
    if business == "out_of_stock":
        # The transaction was rolled back, so the cart is intact. Drop the
        # unavailable line so the user can retry, as a real shop would.
        update_cart_item(conn, user_id, ref, 0)
    return Outcome(data={"order_id": ref} if business == "ok" else None,
                   business=business, retries=retries,
                   extra={"removed": ref} if business == "out_of_stock" else {})


def order_history(conn, user_id: int) -> Outcome:
    orders = conn.execute(
        "SELECT id, total_cents, item_count, created_ms FROM orders WHERE user_id = ? "
        "ORDER BY created_ms DESC LIMIT 10", [user_id]).rows
    items = []
    if orders:
        items = conn.execute(
            "SELECT oi.product_id, oi.qty, oi.unit_price_cents, p.name FROM order_items oi "
            "JOIN products p ON p.id = oi.product_id WHERE oi.order_id = ? LIMIT 100",
            [orders[0][0]]).rows
    return Outcome(data=[o[0] for o in orders], rows_read=len(orders) + len(items))


def write_review(conn, user_id: int, product_id: int, stars: int, body: str) -> Outcome:
    def unit(tx):
        tx.execute("INSERT INTO reviews (product_id, user_id, stars, body, created_ms) "
                   "VALUES (?, ?, ?, ?, ?)", [product_id, user_id, stars, body, now_ms()])
        affected = tx.execute(
            "UPDATE products SET rating_sum = rating_sum + ?, rating_count = rating_count + 1 "
            "WHERE id = ?", [stars, product_id]).affected
        return "ok" if affected else "not_found"
    business, retries = run_transaction(conn, unit)
    return Outcome(business=business, retries=retries)


# ------------------------------------------------------------------ admin --


def admin_dashboard(conn, since_ms: int) -> Outcome:
    by_category = conn.execute(
        "SELECT category, count(*) AS n, sum(sold) AS sold FROM products "
        "GROUP BY category ORDER BY sold DESC LIMIT 50").rows
    recent = conn.execute(
        "SELECT count(*) AS n, sum(total_cents) AS revenue FROM orders WHERE created_ms >= ?",
        [since_ms]).one
    low_stock = conn.execute(
        "SELECT count(*) AS n FROM products WHERE stock < 10").scalar
    return Outcome(data={"categories": len(by_category), "recent_orders": recent[0],
                         "revenue": recent[1], "low_stock": low_stock},
                   rows_read=len(by_category) + 2)


def restock(conn, product_ids: list[int], amount: int) -> Outcome:
    def unit(tx):
        for product_id in product_ids:
            tx.execute("UPDATE products SET stock = stock + ? WHERE id = ?", [amount, product_id])
        tx.execute("INSERT INTO audit_log (user_id, action, detail, at_ms) VALUES (0, 'restock', ?, ?)",
                   [",".join(map(str, sorted(product_ids))), now_ms()])
    _, retries = run_transaction(conn, unit)
    return Outcome(retries=retries)


# ------------------------------------------------------------- invariants --


def invariants(conn, initial_stock_by_id: dict[int, int]) -> dict:
    """Global conservation laws that hold if no write was lost or torn.

    * stock is never negative (the guarded UPDATE must hold under races);
    * for every product ``initial_stock == stock + sold - restocked``, where
      restocks are read back from the audit log;
    * units sold == units in order lines == units summed in order headers;
    * loyalty credits == 10 per paid order.
    """
    problems: list[str] = []
    negative = conn.execute("SELECT count(*) FROM products WHERE stock < 0").scalar
    if negative:
        problems.append(f"{negative} products with negative stock")

    # Restocks are read back grouped by their detail string ("id,id,id" with
    # sorted ids): at most C(20, 3) = 1 140 distinct groups whatever the event
    # count (older runs stored unsorted ids: at most 6 840 groups).
    restocked: dict[int, int] = {}
    groups = conn.execute(
        "SELECT detail, count(*) FROM audit_log WHERE action = 'restock' GROUP BY detail LIMIT 9000"
    ).rows
    if len(groups) >= 9000:
        problems.append("too many distinct restock groups to verify stock conservation")
    for detail, events in groups:
        for pid in detail.split(","):
            restocked[int(pid)] = restocked.get(int(pid), 0) + 100 * events
    drift = 0
    for pid, stock, sold in conn.execute(
            "SELECT id, stock, sold FROM products LIMIT 10000").rows:
        if initial_stock_by_id.get(pid) is None:
            continue
        if initial_stock_by_id[pid] + restocked.get(pid, 0) != stock + sold:
            drift += 1
    if drift:
        problems.append(f"{drift} products break stock + sold == initial + restocked")

    sold = conn.execute("SELECT sum(sold) FROM products").scalar or 0
    line_units = conn.execute("SELECT sum(qty) FROM order_items").scalar or 0
    header_units = conn.execute("SELECT sum(item_count) FROM orders").scalar or 0
    if not (sold == line_units == header_units):
        problems.append(f"units sold {sold} != order lines {line_units} != headers {header_units}")

    paid = conn.execute("SELECT count(*) FROM orders WHERE status = 'paid'").scalar or 0
    credits = conn.execute("SELECT sum(credits) FROM users").scalar or 0
    if credits != paid * LOYALTY_POINTS_PER_ORDER:
        problems.append(f"credits {credits} != {LOYALTY_POINTS_PER_ORDER} x {paid} paid orders")

    counts = {t: conn.execute(f"SELECT count(*) FROM {t}").scalar
              for t, _ in schema.TABLES}
    return {"ok": not problems, "problems": problems, "row_counts": counts,
            "units_sold": sold, "paid_orders": paid}
