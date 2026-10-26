- `model_config json_encoders={type: function}` (pydantic 2 still applies it): the JSON-mode serializer of each field of
  that type (inherited by subclasses; a field's own serializer wins). The type inside `Optional[...]` or a container
  is refused.
