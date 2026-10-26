- `defaultdict(lambda: <literal>)` (a dict or list display, `defaultdict(int)`, a constant...): each missing key gets
  a fresh copy of the literal. A defaultdict's `.copy()` and `copy.copy()` keep its factory (they lost it).
