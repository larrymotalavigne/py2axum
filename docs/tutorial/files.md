# Forms and files

FastAPI reads form fields and uploaded files with `Form()`, `File()` and `UploadFile`, from multipart or
URL-encoded bodies. The binary parses the same bodies and validates them with FastAPI's rules and errors.

Form fields with a constraint, one uploaded file with a form field next to it, a list of files, and
a file read as `bytes`. Like every example on this site, it is compiled and compared with FastAPI in CI ([how](testing.md)).

```python title="docs_src/tutorial/files.py"
--8<-- "docs_src/tutorial/files.py"
```

!!! note "Left to Python in this example"
    `POST /files/raw` declares `data: Annotated[bytes, File()]`: py2axum only translates `File()` parameters
    typed `UploadFile` or `list[UploadFile]`, so `py2axum check` reports it and `--python-side auto` relays it.
    Use `UploadFile` and `await file.read()` to keep it native.

## What is native

- `Form()`/`File()`/`UploadFile` (single, optional or list, a literal default such as `File(default=[])`, a
  fresh copy per request; multipart via `multer` or urlencoded; an empty
  string counts as absent like FastAPI; `UploadFile` in memory: `read`, `seek`, `filename`,
  `content_type`, `size`, `.file`).
- `OAuth2PasswordRequestForm` is a form too ([Security](security.md)).
- A `File()` parameter typed `bytes`, `bytes | None` or `list[bytes]` gets the uploaded file's content, as
  FastAPI's `_extract_form_body` (a text field is validated as is, Pydantic encoding a `str`; a text field in a
  `list[bytes]` is FastAPI's 500, `'str' object has no attribute 'read'`). `bytes` follows Pydantic: a `str` is
  encoded to UTF-8 (lax), `min_length`/`max_length` count bytes, JSON output is the UTF-8 text (invalid UTF-8:
  `PydanticSerializationError`, a 500).
- A model of form fields (`Annotated[Model, Form()]`): its fields read from the form, every other field added,
  then validated as the model at `["body"]` (FastAPI's `_extract_form_body`); refused next to another form or
  body parameter.

Sending a file back is `FileResponse` ([Responses](responses.md)); reading files on the server uses `pathlib`
and `open()` ([Standard library](../reference/stdlib.md)). Multipart limits and the handling of uploaded file
names are in [Security § Request input](../advanced/security.md#request-input) and
[Security § Files and paths](../advanced/security.md#files-and-paths).

## What stays in Python

- A `File()` parameter of another type than `UploadFile`, `bytes` or a list of them is refused.
