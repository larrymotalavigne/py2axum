"""a revision reading the database: refused (the binary will not apply it)

Revision ID: 0004
Revises: 0003
"""
import sqlalchemy as sa
from alembic import op

revision = "0004"
down_revision = "0003"


def upgrade():
    bind = op.get_bind()
    if "notes" not in sa.inspect(bind).get_table_names():
        notes = op.create_table("notes", sa.Column("id", sa.Integer, primary_key=True), sa.Column("data", sa.JSON))
        op.bulk_insert(notes, [{"id": 1, "data": None}, {"id": 2, "data": {"a": [1, "é"]}}])


def downgrade():
    pass
