"""Scenario of fixtures/pdfmergeapp: `Path.cwd()` / `os.getcwd()` (both servers start from the repository root).
No database."""

STEPS: list = [("GET", "/health", None), ("GET", "/cwd", None)]


def reset(db: str) -> None:
    pass
