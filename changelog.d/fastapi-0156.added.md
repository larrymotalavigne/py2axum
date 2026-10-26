- FastAPI up to 0.162.0, Starlette up to 1.10.0 and SQLAlchemy 2.1.6 are tested (the highest versions moved from
  0.142.3, 1.7.0 and 2.1.4; 0.142.3 and 0.156.8 stay tested on langapp). The binary follows the request handling of the version the project locks: from
  0.144 non-finite floats of a JSON Lines item are null and a text field of a `list[bytes]` file parameter is kept;
  from 0.147 to 0.161 a missing `bool` form field is False (0.162 dropped it); from 0.148 defaults are applied at validation (not validated,
  and left out of a parameter model's input); from 0.149 an empty form value is absent for a form model's fields
  too; from 0.150 an empty file input is absent; from 0.151 a JSON `null` body is validated (an embedded body
  reports each field missing); from 0.152 a header model's errors name the headers. Starlette 1.8's
  TrustedHostMiddleware compares host names case-insensitively.
