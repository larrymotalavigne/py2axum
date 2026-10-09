# Testing

FastAPI applications are tested with pytest and `TestClient` (or `httpx.AsyncClient`) against the Python
application. With py2axum nothing changes there: the Python application stays the source of truth, and your
tests, local development and debugging keep running on Python. The binary is a build artifact, and it is tested
against the Python application itself, by differential conformance.

The examples of this site are tested this way: `docs_src/compare.sh` translates `docs_src/`, builds
the binary, starts it next to the FastAPI application on the same database (the binary relays the three
routes left to Python), and plays this scenario on both; every response must be identical, in a normal pass
and with list streaming forced.

```python title="docs_src/scenario.py"
--8<-- "docs_src/scenario.py"
```

```bash
createdb py2axum_docs
pip install py2axum -r docs_src/requirements.txt httpx websockets
DATABASE_URL=postgresql://postgres@127.0.0.1/py2axum_docs docs_src/compare.sh
```

## Testing the binary

A conformance run sends the same requests to FastAPI and to the binary, on the same data, and compares the
responses: status, content type and encoding, the headers that matter, and the body (JSON with key order
kept, anything else byte for byte). There are three ways to produce the requests:

1. **A scenario**: requests you write, played on both servers by `tests/conformance.py`.
2. **Replay**: traffic recorded in front of your Python application by `py2axum.record`, replayed on both.
3. **Generation**: requests derived from your OpenAPI schema, valid and invalid.

The [bookshelf example](https://github.com/larrymotalavigne/py2axum/tree/main/examples/bookshelf) ships its
own scenario and a `compare.sh` that runs it ([First application](../getting-started/first-app.md#compare-the-binary-with-the-python-application)).
How to write a scenario, run it in CI, record and replay traffic, and generate requests is explained in
[Conformance and replay](../advanced/conformance.md).

## What is native

Nothing to translate: tests are not compiled. `TestClient` and pytest fixtures stay in your Python test suite.

## What stays in Python

Your whole test suite.
