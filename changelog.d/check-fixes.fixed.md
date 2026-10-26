- `py2axum check` reports a whole-application error once (a syntax error was repeated for each pass), and a string
  with a lone surrogate (`re.compile('[\ud800-\udfff]')`) is refused at its line instead of crashing the check.
