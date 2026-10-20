"""pgpy 0.6.0's `PGPKey.from_blob()` (dynrt/pgpy.rs, pgpycore.rs): a public-key upload as an application validates it
(the module imported lazily behind an `imghdr` stub, Python 3.13 removed imghdr), and the details of what it parses
(user ids in pgpy's order, fingerprint, the second element of the tuple, the exceptions). No database.

Generated with `python -m py2axum fixtures/pgpyapp --root . -o generated/pgpyapp_axum --name pgpyapp_axum` from an
environment where pgpy is installed (it is never imported by the transpiler).
"""
