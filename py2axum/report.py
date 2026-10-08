"""Coverage report: which routes of a real project translate, and what blocks the others.

    python -m py2axum <package> [--root DIR] --report report.md     (also writes report.json)

Two sources are combined for each route:
- the transpiler itself, run in collect mode (never stops at the first error): a route is
  *translated* only if its handler really generates;
- a static scanner listing every construction of the route that the subset does not cover
  (signature, body, models and schemas reached, project functions called), because translation
  stops at the first error and the goal is to know *everything* a route needs.
"""
from __future__ import annotations

import json
import re
from collections import Counter
from dataclasses import dataclass, field
from pathlib import Path

from .frontend import Frontend
from .ir import TranspileError


# dyn compiler message -> construction label (French for the internal report, English for `check`).
# Order matters (first match wins).
ERROR_CLASSES: list[tuple[str, str, str]] = [
    (r"library call `([a-zA-Z0-9_]+)\.?[^`]*\(\)` is not supported", "lib \\1", "library \\1"),
    (r"library value `([a-zA-Z0-9_]+)[^`]*` is not supported", "lib \\1", "library \\1"),
    (r"middleware (\S+) is not supported", "middleware \\1", "middleware \\1"),
    (r"method \.(\w+)\(\) is not implemented by the runtime", "méthode .\\1() absente du runtime (500 exécution)", "method .\\1() not in the runtime"),
    (r"\.(\w+)\((\w+)=\) is not supported \(only", ".\\1(\\2=) (500 exécution)", ".\\1(\\2=)"),
    (r"attribute \.(\w+) is not provided by the runtime", "attribut .\\1 absent du runtime (500 exécution)", "attribute .\\1 not in the runtime"),
    (r"@app\.(\w+)\(\.\.\.\) is not supported", "@app.\\1", "@app.\\1"),
    (r"app\.(\w+)\(\.\.\.\) is not supported", "app.\\1()", "app.\\1()"),
    (r"FastAPI\(lifespan=", "lifespan (à déclarer --python-side)", "lifespan"),
    (r"include_router: cannot resolve", "include_router non résolu", "unresolved include_router"),
    (r"syntax error", "erreur de syntaxe", "syntax error"),
    (r"coroutine `.*` is not awaited", "coroutine non attendue (create_task/gather)", "coroutine not awaited"),
    (r"decorator @(\S+) on", "décorateur @\\1", "decorator @\\1"),
    (r"(Cookie|Form|File)\(\) parameters", "paramètre \\1()", "\\1() parameter"),
    (r"dependency `.*` must be a project function", "dépendance externe (Depends(objet))", "non-project dependency"),
    (r"generator dependencies", "dépendance générateur", "generator dependency"),
    (r"only exception classes are supported", "classe projet (hors modèle/schéma/exception)", "project class"),
    (r"is not a Pydantic model \(enums", "type Enum ou classe non Pydantic", "Enum or non-Pydantic type"),
    (r"unsupported type annotation `(?:[\w.]*\.)?(UUID|Decimal|EmailStr|HttpUrl|SecretStr|AnyUrl)", "type \\1", "type \\1"),
    (r"unsupported type annotation", "annotation de type non supportée", "type annotation"),
    (r"column .*: unsupported column type `(?:[\w.]*\.)?(\w+)", "colonne \\1", "column type \\1"),
    (r"column .*: (callable/SQL defaults|only onupdate)", "défaut de colonne calculé", "computed column default"),
    (r"column .*: cannot infer", "colonne sans type", "untyped column"),
    (r"inheritance between mapped classes", "héritage de modèles", "model inheritance"),
    (r"abstract models", "modèle abstrait", "abstract model"),
    (r"exactly one primary key", "clé primaire composite", "composite primary key"),
    (r"@(model_validator|computed_field|field_serializer|model_serializer)", "Pydantic @\\1", "Pydantic @\\1"),
    (r"only mode='after' field validators", "validateur Pydantic mode before/wrap", "Pydantic validator mode"),
    (r"class `Config`", "Pydantic v1 Config", "Pydantic v1 Config"),
    (r"model_config (\w+)= is not supported", "model_config \\1=", "model_config \\1="),
    (r"class attribute `.*` is not supported", "attribut de classe de schéma", "schema class attribute"),
    (r"unsupported option (\w+)= in", "option \\1=", "option \\1="),
    (r"unsupported route option (\w+)=", "option de route \\1=", "route option \\1="),
    (r"unsupported statement `(\w+)`", "instruction \\1", "statement \\1"),
    (r"`global`/`nonlocal`", "global/nonlocal", "global/nonlocal"),
    (r"starred assignment|`\*`/`\*\*` expansion", "expansion */** non supportée", "*/** expansion"),
    (r"unsupported exception class", "classe d'exception externe", "library exception class"),
    (r"builtin `(\w+)\(\)` is not supported", "builtin \\1()", "builtin \\1()"),
    (r"unsupported await", "await non supporté", "await"),
    (r"is not a constant", "valeur non constante", "non-constant value"),
    (r"needs a type annotation", "paramètre sans annotation", "unannotated parameter"),
    (r"must be a module-level function", "endpoint imbriqué", "nested endpoint"),
    (r"unsupported expression", "expression non supportée", "expression"),
    (r"with/else|for/else|while/else", "boucle avec else", "loop with else"),
    (r"unknown name", "nom inconnu", "unknown name"),
    (r"module .* has no attribute", "attribut de module introuvable", "missing module attribute"),
    (r"erreur interne", "erreur interne du transpileur", "internal transpiler error"),
]


def classify(e: TranspileError, english: bool = False) -> tuple[str, str]:
    for pat, label, en in ERROR_CLASSES:
        m = re.search(pat, e.msg)
        if m:
            return m.expand(en if english else label), "x"
    return ("other: " if english else "autre : ") + re.sub(r"`[^`]*`", "`…`", e.msg)[:80], "other"


@dataclass
class RouteReport:
    method: str
    path: str
    func: str
    where: str
    conditional: bool
    status: str = "bloquée"  # traduite | bloquée
    error: str | None = None
    blockers: dict[str, list[str]] = field(default_factory=dict)  # label -> [file:line]


def _where(e: TranspileError) -> str:
    if e.file and e.node is not None and hasattr(e.node, "lineno"):
        return f"{e.file}:{e.node.lineno}"
    return e.file or "?"


def collect_dyn(package: Path, root: Path | None, python_side, auto: bool = False) -> tuple[Frontend, list]:
    """Dyn pass that never stops: the frontend (global errors) and, per route, its info and errors (its own and
    those of the functions it reaches). auto: raw `add_route` with a literal path go to `fe.auto_side`."""
    import tempfile

    from . import dyn

    fe = Frontend(package, root, collect=True)
    if auto:
        fe.auto_side = {}
    for path, e in fe.index.syntax_errors:
        fe.global_errors.append(TranspileError(f"syntax error: {e.msg}", None, f"{path}:{e.lineno}"))
    dyn.prepare(fe, set(python_side))
    with tempfile.TemporaryDirectory() as tmp:
        proj = dyn.generate_project(fe, Path(tmp), str(package), "check", collect=True)
    return fe, [(info, ([err] if err else []) + dyn.closure_errors(proj, info["id"]))
                for info, err in getattr(proj, "route_infos", [])]


def build(package: Path, root: Path | None = None, stream: bool = True, python_side=("lifespan",)) -> dict:
    """Per-route coverage."""
    fe, per_route = collect_dyn(package, root, python_side)
    routes = []
    for info, errs in per_route:
        rr = RouteReport(info["method"].upper(), info["path"], info["func"], f"{info['file']}:{info['line']}",
                         info["conditional"], "bloquée" if errs else "traduite")
        if errs:
            rr.error = errs[0].render()
        for e in errs:
            label, _ = classify(e)
            rr.blockers.setdefault(label, [])
            w = _where(e)
            if w not in rr.blockers[label]:
                rr.blockers[label].append(w)
        routes.append(rr)
    routes.sort(key=lambda r: (r.path, r.method))
    return {
        "package": str(package),
        "routes": routes,
        "global": [(classify(e)[0], e.render()) for e in fe.global_errors],
        "poisoned": [],
        "notes": fe.notes,
        "backend": "dyn",
    }


def aggregate(routes: list[RouteReport]) -> dict:
    blocked = [r for r in routes if r.status != "traduite"]
    touched = Counter()
    occurrences = Counter()
    alone = Counter()
    for r in blocked:
        for label, wheres in r.blockers.items():
            touched[label] += 1
            occurrences[label] += len(wheres)
        if len(r.blockers) == 1:
            alone[next(iter(r.blockers))] += 1
    # greedy: at each step implement the construction that completes the most routes;
    # when none completes any route, the one blocking the most remaining routes.
    remaining = [set(r.blockers) for r in blocked]
    done: set[str] = set()
    steps = []
    translated = len(routes) - len(blocked)
    while any(remaining) and len(steps) < 40:
        cand = {lbl for s in remaining for lbl in s} - done
        if not cand:
            break

        def gain(lbl):
            return sum(1 for s in remaining if s and s <= done | {lbl})

        best = max(cand, key=lambda lbl: (gain(lbl), sum(lbl in s for s in remaining), lbl))
        g = gain(best)
        done.add(best)
        translated += g
        remaining = [s if not s <= done else set() for s in remaining]
        steps.append({"construction": best, "nouvelles": g, "cumul": translated})
    table = [
        {"construction": lbl, "occurrences": occurrences[lbl], "routes_touchées": touched[lbl],
         "débloquées_seule": alone[lbl]}
        for lbl in sorted(touched, key=lambda x: (-alone[x], -touched[x], x))
    ]
    return {"total": len(routes), "traduites": len(routes) - len(blocked), "bloquées": len(blocked),
            "constructions": table, "glouton": steps}


def write(result: dict, md_path: Path) -> None:
    routes: list[RouteReport] = result["routes"]
    agg = aggregate(routes)
    data = {
        "package": result["package"],
        "résumé": {k: agg[k] for k in ("total", "traduites", "bloquées")},
        "bloquants_globaux": [{"construction": a, "erreur": b} for a, b in result["global"]],
        "entités_non_traduisibles": [{"entité": a, "construction": b, "erreur": c} for a, b, c in result["poisoned"]],
        "constructions": agg["constructions"],
        "glouton": agg["glouton"],
        "notes": result["notes"],
        "routes": [r.__dict__ for r in routes],
    }
    md_path.with_suffix(".json").write_text(json.dumps(data, ensure_ascii=False, indent=1))
    o = [f"# Rapport de couverture py2axum — `{result['package']}`", ""]
    o.append(f"**{agg['traduites']} / {agg['total']} routes traduites**, {agg['bloquées']} bloquées.")
    o.append("")
    if result["global"]:
        o += ["## Bloquants globaux (application entière)", ""]
        for label, err in result["global"]:
            o.append(f"- **{label}** — `{err}`")
        o.append("")
    o += ["## Constructions bloquantes", "",
          "« seule » = routes dont c'est l'unique bloquant (débloquées dès qu'elle est implémentée).", "",
          "| Construction | Routes touchées | Seule | Occurrences |", "|---|---:|---:|---:|"]
    for row in agg["constructions"]:
        o.append(f"| {row['construction']} | {row['routes_touchées']} | {row['débloquées_seule']} | "
                 f"{row['occurrences']} |")
    o += ["", "## Ordre glouton (implémenter dans cet ordre)", "",
          "| # | Construction | Routes en plus | Cumul traduites |", "|---:|---|---:|---:|"]
    for i, s in enumerate(agg["glouton"], 1):
        o.append(f"| {i} | {s['construction']} | {s['nouvelles']} | {s['cumul']} / {agg['total']} |")
    if result["poisoned"]:
        o += ["", "## Modèles et schémas non traduisibles", "", "| Entité | Construction | Erreur |", "|---|---|---|"]
        for a, b, c in result["poisoned"]:
            o.append(f"| {a} | {b} | `{c}` |")
    o += ["", "## Routes", "", "| Statut | Méthode | Chemin | Handler | Bloquants |", "|---|---|---|---|---|"]
    for r in routes:
        mark = "✅" if r.status == "traduite" else "⛔"
        cond = " (conditionnelle)" if r.conditional else ""
        o.append(f"| {mark} | {r.method} | `{r.path}`{cond} | `{r.func}` | {', '.join(sorted(r.blockers))} |")
    if result["notes"]:
        o += ["", "## Notes", ""] + [f"- {n}" for n in result["notes"]]
    md_path.write_text("\n".join(o) + "\n")
