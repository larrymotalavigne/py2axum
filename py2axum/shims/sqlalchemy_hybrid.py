"""`sqlalchemy.ext.hybrid.Comparator` (SQLAlchemy 2.1) written in the subset py2axum translates: a project class
deriving from it (a hybrid's custom comparator, a hybrid value object) is translated with this class as its base.
Loaded by `ModuleIndex.load_shim`, never imported. What SQLAlchemy's class does: `__init__(expression)`,
`__clause_element__()`, and the comparison operators of `ColumnOperators` sent to `operate(op, other)` (whose
default raises NotImplementedError); hashed by identity (`ColumnOperators.__hash__ = Operators.__hash__`).
Its other operators (like(), in_(), +, ...) are not modelled: a read of one is an AttributeError."""
import operator


class Comparator:
    def __init__(self, expression):
        self.expression = expression

    def __clause_element__(self):
        expr = self.expression
        if hasattr(expr, "__clause_element__"):
            return expr.__clause_element__()
        return expr

    def operate(self, op, *other, **kwargs):
        raise NotImplementedError(str(op))

    def reverse_operate(self, op, other, **kwargs):
        raise NotImplementedError(str(op))

    def __eq__(self, other):
        return self.operate(operator.eq, other)

    def __ne__(self, other):
        return self.operate(operator.ne, other)

    def __lt__(self, other):
        return self.operate(operator.lt, other)

    def __le__(self, other):
        return self.operate(operator.le, other)

    def __gt__(self, other):
        return self.operate(operator.gt, other)

    def __ge__(self, other):
        return self.operate(operator.ge, other)

    __hash__ = object.__hash__
