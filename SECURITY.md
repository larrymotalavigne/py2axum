# Security policy

py2axum generates the code you deploy, so a translation that differs from Python's behaviour on
authentication, authorization, validation or SQL generation can be a security issue in your application.

Please report such issues privately through GitHub's
[security advisories](https://github.com/larrymotalavigne/py2axum/security/advisories/new) rather than in a
public issue. Include the Python construct, the expected (Python) behaviour and what the binary does. You
should get an answer within a week.

Only the latest release receives fixes. What the generated server guarantees against untrusted clients, and
how it differs from uvicorn and Starlette, is described in [the security documentation](https://larrymotalavigne.github.io/py2axum/advanced/security/).
