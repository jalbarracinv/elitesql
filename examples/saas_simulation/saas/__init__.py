"""Mini-SaaS backend used by the concurrency simulation.

The package is deliberately small and portable: `schema.py` owns the DDL and
the seed, `service.py` implements the business operations against a driver,
and `drivers.py` adapts EliteSQL (embedded or sidecar) and SQLite to the
handful of primitives the service needs.
"""
