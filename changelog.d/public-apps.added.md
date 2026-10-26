- Patterns of public applications (Netflix Dispatch...): an enum deriving from a common enum without members
  (`class AppEnum(StrEnum): pass`), extra keywords of pydantic's `Field()` (`Field(True, nullable=True)`: its
  json_schema_extra, the schema only).
