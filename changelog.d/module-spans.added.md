- A module variable bound by several module-level statements (`X = os.getenv(...)` then `if not X: X = ...`):
  the statements from its first binding to its last run in order at startup.
