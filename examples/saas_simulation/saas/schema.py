"""Schema and deterministic seed of the mini-SaaS.

Domain: a small e-commerce SaaS. Tenants share one database; every row that a
user owns carries a `user_id`. Products carry a 32-dimensional embedding so the
"recommendations" operation exercises the native HNSW index, and a text index
on `products.description` backs the full-text search operation.
"""

from __future__ import annotations

import hashlib
import math
import random

EMBEDDING_DIM = 32
CATEGORIES = [
    "audio", "books", "camera", "garden", "gaming", "home", "kitchen",
    "office", "outdoor", "pets", "sports", "toys", "tools", "wearables",
]
ADJECTIVES = ["compact", "wireless", "premium", "basic", "smart", "rugged",
              "portable", "classic", "modern", "eco", "pro", "mini", "ultra"]
NOUNS = ["speaker", "lamp", "kettle", "tent", "keyboard", "headphones", "mug",
         "backpack", "tripod", "monitor", "drill", "planter", "watch", "puzzle",
         "leash", "notebook", "blender", "bottle", "scale", "router"]
WORDS = ("quality durable design comfortable battery fast lightweight quiet "
         "bright warranty steel bamboo cotton waterproof rechargeable family "
         "travel kitchen office garden studio outdoor compact silent").split()

# One source of truth for the request mix. The concurrency generator and the
# single-operation benchmark intentionally consume the same weights; callers
# may normalize a selected subset, but must print which subset they used.
OPERATION_WEIGHTS = {
    "browse": 20.0,
    "product_detail": 17.0,
    "session_check": 12.0,
    "add_to_cart": 10.0,
    "search_text": 8.0,
    "view_cart": 8.0,
    "recommend": 7.0,
    "checkout": 4.0,
    "update_cart_item": 3.0,
    "order_history": 3.0,
    "write_review": 2.0,
    "relogin": 1.5,
    "update_profile": 1.0,
    "admin_dashboard": 1.0,
    "signup": 0.5,
    "restock": 0.3,
}

# Kept solely to reproduce reports made before metric v2. It is not a full
# workload: its weights add to 89 and the old script divided by 100.
HISTORICAL_OPS_COST_WEIGHTS = {
    "browse": 20.0,
    "product_detail": 17.0,
    "session_check": 12.0,
    "add_to_cart": 10.0,
    "search_text": 8.0,
    "view_cart": 8.0,
    "recommend": 7.0,
    "order_history": 3.0,
    "write_review": 2.0,
    "update_profile": 1.0,
    "admin_dashboard": 1.0,
}

# Portable DDL: `?` placeholders everywhere, `AUTO_INCREMENT` integer ids,
# `text` for strings. The SQLite driver rewrites the few type names it does
# not know (see drivers.py) and drops the vector column.
TABLES = [
    ("users", """
        CREATE TABLE users (
          id int AUTO_INCREMENT PRIMARY KEY,
          email text NOT NULL,
          password_hash text NOT NULL,
          display_name text NOT NULL,
          plan text NOT NULL DEFAULT 'free',
          credits int NOT NULL DEFAULT 0,
          login_count int NOT NULL DEFAULT 0,
          created_at timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP
        )"""),
    ("sessions", """
        CREATE TABLE sessions (
          token text NOT NULL,
          user_id int NOT NULL,
          created_at timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP,
          last_seen_ms int NOT NULL
        )"""),
    ("products", f"""
        CREATE TABLE products (
          id int AUTO_INCREMENT PRIMARY KEY,
          sku text NOT NULL,
          name text NOT NULL,
          description text NOT NULL,
          category text NOT NULL,
          price_cents int NOT NULL,
          stock int NOT NULL,
          sold int NOT NULL DEFAULT 0,
          rating_sum int NOT NULL DEFAULT 0,
          rating_count int NOT NULL DEFAULT 0,
          embedding vector({EMBEDDING_DIM})
        )"""),
    ("carts", """
        CREATE TABLE carts (
          id int AUTO_INCREMENT PRIMARY KEY,
          user_id int NOT NULL,
          status text NOT NULL DEFAULT 'open',
          updated_ms int NOT NULL
        )"""),
    ("cart_items", """
        CREATE TABLE cart_items (
          id int AUTO_INCREMENT PRIMARY KEY,
          cart_id int NOT NULL,
          product_id int NOT NULL,
          qty int NOT NULL,
          unit_price_cents int NOT NULL
        )"""),
    ("orders", """
        CREATE TABLE orders (
          id int AUTO_INCREMENT PRIMARY KEY,
          user_id int NOT NULL,
          status text NOT NULL,
          total_cents int NOT NULL,
          item_count int NOT NULL,
          created_ms int NOT NULL
        )"""),
    ("order_items", """
        CREATE TABLE order_items (
          id int AUTO_INCREMENT PRIMARY KEY,
          order_id int NOT NULL,
          product_id int NOT NULL,
          qty int NOT NULL,
          unit_price_cents int NOT NULL
        )"""),
    ("reviews", """
        CREATE TABLE reviews (
          id int AUTO_INCREMENT PRIMARY KEY,
          product_id int NOT NULL,
          user_id int NOT NULL,
          stars int NOT NULL,
          body text NOT NULL,
          created_ms int NOT NULL
        )"""),
    ("audit_log", """
        CREATE TABLE audit_log (
          id int AUTO_INCREMENT PRIMARY KEY,
          user_id int NOT NULL,
          action text NOT NULL,
          detail text,
          at_ms int NOT NULL
        )"""),
]

INDEXES = [
    "CREATE UNIQUE INDEX ON users (email)",
    "CREATE UNIQUE INDEX ON sessions (token)",
    "CREATE INDEX ON sessions (user_id)",
    "CREATE UNIQUE INDEX ON products (sku)",
    "CREATE INDEX ON products (category)",
    "CREATE INDEX ON carts (user_id)",
    "CREATE INDEX ON cart_items (cart_id)",
    "CREATE INDEX ON orders (user_id)",
    "CREATE INDEX ON order_items (order_id)",
    "CREATE INDEX ON reviews (product_id)",
    "CREATE INDEX ON audit_log (user_id)",
]


def password_hash(email: str, password: str) -> str:
    # Deliberately cheap: a bcrypt-class KDF would make the CPU cost of the
    # load generator dominate the measurement instead of the database.
    return hashlib.sha256(f"{email}:{password}".encode()).hexdigest()


def user_email(i: int) -> str:
    return f"user{i}@example.com"


def user_password(i: int) -> str:
    return f"pw-{i}-secret"


def product_embedding(rng: random.Random, category_index: int) -> list[float]:
    """A unit vector clustered by category, so neighbours are meaningful."""
    vec = [rng.gauss(0.0, 0.35) for _ in range(EMBEDDING_DIM)]
    vec[category_index % EMBEDDING_DIM] += 1.0
    vec[(category_index * 7 + 3) % EMBEDDING_DIM] += 0.6
    norm = math.sqrt(sum(x * x for x in vec)) or 1.0
    return [round(x / norm, 5) for x in vec]


def generate_products(count: int, seed: int = 7) -> list[dict]:
    rng = random.Random(seed)
    products = []
    for i in range(count):
        ci = rng.randrange(len(CATEGORIES))
        name = f"{rng.choice(ADJECTIVES)} {rng.choice(NOUNS)} {i}"
        description = " ".join(rng.choice(WORDS) for _ in range(12))
        products.append({
            "sku": f"SKU-{i:06d}",
            "name": name,
            "description": f"{name}: {description}",
            "category": CATEGORIES[ci],
            "price_cents": rng.randrange(199, 49_999),
            # Plenty of stock for most items; a few "hot" items with scarce
            # stock make the checkout conflicts real.
            "stock": 25 if i < 20 else rng.randrange(500, 5_000),
            "embedding": product_embedding(rng, ci),
        })
    return products
