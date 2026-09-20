"""A virtual user: a small state machine that picks *different* operations.

Each virtual user owns one account, keeps its session token and a local model
of its own cart (nobody else touches that cart), so after every cart write it
can verify read-your-writes when it views the cart. The operation mix is
weighted to look like a shop: three quarters reads, one quarter writes, with a
skewed product popularity so some rows are genuinely hot.
"""

from __future__ import annotations

import random
from typing import Optional

from . import schema, service
from .drivers import Conflict, NotSupported

WEIGHTS = dict(schema.OPERATION_WEIGHTS)
# Experiments: SAAS_SIM_DROP_OPS="search_text,recommend" removes operations from the mix.
for _dropped in filter(None, __import__("os").environ.get("SAAS_SIM_DROP_OPS", "").split(",")):
    WEIGHTS.pop(_dropped.strip(), None)

READ_OPS = {"browse", "product_detail", "session_check", "search_text", "view_cart",
            "recommend", "order_history", "admin_dashboard"}
HOT_PRODUCTS = 20          # ids 1..20 start with 25 units each (schema.generate_products)
SEARCH_TERMS = ["bamboo kettle", "quiet speaker", "waterproof tent", "steel bottle",
                "office lamp", "travel backpack", "garden planter", "smart watch",
                "rechargeable drill", "cotton family", "compact monitor", "silent router"]


class VirtualUser:
    def __init__(self, index: int, account: int, product_count: int, seed: int,
                 run_tag: str):
        self.index = index
        self.account = account
        self.rng = random.Random(seed * 1_000_003 + index)
        self.products = product_count
        self.run_tag = run_tag
        self.token: Optional[str] = None
        self.user_id: Optional[int] = None
        self.expected_cart: dict[int, int] = {}
        self.cart_known = False          # becomes True after the first view or checkout
        self.consistency_violations = 0
        self.signups = 0
        self._ops = list(WEIGHTS)
        self._weights = list(WEIGHTS.values())

    # -- choices -----------------------------------------------------------
    def pick_product(self, for_cart: bool = False) -> int:
        r = self.rng.random()
        if r < (0.10 if for_cart else 0.15):
            return self.rng.randint(1, min(HOT_PRODUCTS, self.products))
        if r < 0.55:                                  # the popular 5 %
            return self.rng.randint(1, max(1, self.products // 20))
        return self.rng.randint(1, self.products)

    def next_op(self) -> str:
        if self.token is None:
            return "login"
        return self.rng.choices(self._ops, self._weights)[0]

    # -- one step ------------------------------------------------------------
    def step(self, conn, op: str) -> service.Outcome:
        rng = self.rng
        if op == "login" or op == "relogin":
            email, password = schema.user_email(self.account), schema.user_password(self.account)
            if op == "relogin" and self.token is not None and rng.random() < 0.5:
                service.logout(conn, self.token)
                self.token = None
            out = service.login(conn, email, password)
            if out.business == "ok":
                self.token, self.user_id = out.data["token"], out.data["user_id"]
                self.cart_known = False
            return out
        if op == "session_check":
            out = service.session_check(conn, self.token)
            if out.business == "not_found":
                # Session vanished (should not happen: only this user logs out).
                self.consistency_violations += 1
                self.token = None
            return out
        if op == "browse":
            return service.browse(conn, rng.choice(schema.CATEGORIES), rng.randrange(0, 5))
        if op == "product_detail":
            return service.product_detail(conn, self.pick_product())
        if op == "search_text":
            return service.search_text(conn, rng.choice(SEARCH_TERMS))
        if op == "recommend":
            return service.recommend(conn, self.pick_product())
        if op == "add_to_cart":
            pid, qty = self.pick_product(for_cart=True), rng.randint(1, 3)
            out = service.add_to_cart(conn, self.user_id, pid, qty)
            if out.business == "ok" and self.cart_known:
                self.expected_cart[pid] = self.expected_cart.get(pid, 0) + qty
            return out
        if op == "view_cart":
            out = service.view_cart(conn, self.user_id)
            if self.cart_known and out.data != self.expected_cart:
                self.consistency_violations += 1
            self.expected_cart = dict(out.data)
            self.cart_known = True
            return out
        if op == "update_cart_item":
            if not self.expected_cart:
                return service.view_cart(conn, self.user_id)
            pid = rng.choice(list(self.expected_cart))
            qty = rng.choice([0, 1, 2, 5])
            out = service.update_cart_item(conn, self.user_id, pid, qty)
            if out.business == "ok" and self.cart_known:
                if qty <= 0:
                    self.expected_cart.pop(pid, None)
                else:
                    self.expected_cart[pid] = qty
            return out
        if op == "checkout":
            out = service.checkout(conn, self.user_id)
            if out.business == "ok":
                self.expected_cart = {}
                self.cart_known = True
            elif out.business == "out_of_stock":
                self.expected_cart.pop(out.extra.get("removed"), None)
            return out
        if op == "order_history":
            return service.order_history(conn, self.user_id)
        if op == "write_review":
            return service.write_review(conn, self.user_id, self.pick_product(),
                                        rng.randint(1, 5), "solid product, would buy again")
        if op == "update_profile":
            return service.update_profile(conn, self.user_id, f"User {self.account} v{rng.randint(1, 99)}")
        if op == "admin_dashboard":
            return service.admin_dashboard(conn, service.now_ms() - 60_000)
        if op == "restock":
            ids = rng.sample(range(1, min(HOT_PRODUCTS, self.products) + 1), 3)
            return service.restock(conn, ids, 100)
        if op == "signup":
            self.signups += 1
            return service.signup(conn, f"new-{self.run_tag}-{self.index}-{self.signups}@example.com",
                                  "pw", f"New {self.index}")
        raise ValueError(op)
