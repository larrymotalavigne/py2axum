"""A dependency factory in its own module: `Depends(parse_list(Query(...), int))` reads the signature of the
function it returns, where the factory's arguments are."""
from fastapi import Query


def parse_list(query: Query, class_type: type):
    """a common shape: a list query parameter given as repeated keys or as one "[a, b]" string"""
    def inner_parse(elements: list[str] = query) -> list[class_type] | None:
        def strip_brackets(text: str) -> str:
            return text.removeprefix("[").removesuffix("]")

        if query.default != Ellipsis and not elements:
            return query.default
        if len(elements) > 1:
            items = elements
        else:
            flat = strip_brackets(elements[0])
            if query.default != Ellipsis and not flat:
                return query.default
            items = [n.strip().strip('"') for n in flat.split(",")]
        errors = {}
        results = []
        for idx, el in enumerate(items):
            try:
                results.append(class_type(el))
            except ValueError as e:
                errors[idx] = repr(e)
        if errors:
            raise ValueError(f"Could not parse elements: {errors}")
        return results

    return inner_parse


def header_flag(name: str = "x-flag", default: bool = False):
    def flag(value: str | None = None) -> bool:
        return default if value is None else value.lower() in ("1", "true", "yes")

    return flag
