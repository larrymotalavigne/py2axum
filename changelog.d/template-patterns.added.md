- The official full-stack FastAPI template translates (19 of its 23 routes; the 4 that send email stay Python-side):
  SQLModel's `UTCDateTime` columns (a `datetime` field from SQLModel 0.0.45: TIMESTAMPTZ, a naive value refused at
  bind time, values read in UTC) and `NaiveDatetime`, `sqlmodel_update(data, update=...)` and
  `Table.model_validate(x, update=...)` on tables of another module, `sqlmodel.col()`, `session.exec(statement)` of
  a `delete(...)` held in a variable, `pwdlib.PasswordHash((Argon2Hasher(...), BcryptHasher(...)))` (the first hasher
  hashes, each verifies its own hashes, `verify_and_update` rehashes with the first), and `warnings.warn()` of a
  builtin category CPython prints (`UserWarning`...: `file:line: Category: message` and the line on stderr, once
  per message and line).
