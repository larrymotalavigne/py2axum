"""Markers defined in one module and used by another (fixtures/integapp/markers.py)."""
from fastapi import Query

LimitQuery = Query(20, ge=1, le=100)
TagsQuery = Query([], alias="tag")
