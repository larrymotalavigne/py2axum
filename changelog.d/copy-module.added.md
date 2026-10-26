- `copy.copy` and `copy.deepcopy` (dynrt/copymod.rs): CPython's semantics (memo for shared and cyclic
  references, unchanged tuples kept, `defaultdict`/`Counter`/`deque` rebuilt, Pydantic models, dataclasses and
  plain classes); classes with their own copy methods refused at translation.
