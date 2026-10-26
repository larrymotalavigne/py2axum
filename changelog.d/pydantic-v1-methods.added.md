- Pydantic v1's deprecated methods on v2 models: `parse_obj`, `parse_raw`, `validate`, `construct`, `from_orm`,
  `schema`, `update_forward_refs`, `.json()`, `.copy(update=, deep=)`.
- `model_config` `validate_default=True` (model-wide, inherited); `defer_build`, `cache_strings`,
  `validate_return` and `use_attribute_docstrings` accepted (no effect on the binary).
