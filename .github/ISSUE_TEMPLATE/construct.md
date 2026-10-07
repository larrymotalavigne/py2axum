---
name: Construct not translated / behaviour differs
about: A Python construct py2axum refuses, or a response that differs from FastAPI
labels: [translation]
---

**Smallest snippet**

```python
# a route showing the construct
```

**What happens**

- py2axum's error (`file:line: ...`), or
- FastAPI's response (status, headers, body) vs the binary's

**Versions**: Python, py2axum, FastAPI, Pydantic, SQLAlchemy (or attach the relevant `uv.lock` entries)
