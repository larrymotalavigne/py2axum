"""tables, an enum created with its table, indexes, seed rows

Revision ID: 0001
Revises:
"""
import sqlalchemy as sa
from alembic import op

revision = "0001"
down_revision = None


def upgrade():
    status = sa.Enum("open", "closed", name="status")
    t = op.create_table(
        "tickets",
        sa.Column("id", sa.Integer, primary_key=True),
        sa.Column("title", sa.String(200), nullable=False),
        sa.Column("status", status, nullable=False, server_default="open"),
        sa.Column("created_at", sa.DateTime(timezone=True), server_default=sa.func.now()),
        sa.Column("payload", sa.Text),
    )
    op.create_index("ix_tickets_title", "tickets", ["title"], unique=True)
    op.bulk_insert(t, [{"id": 1, "title": "first", "status": "open", "payload": None},
                       {"id": 2, "title": "l'apostrophe", "status": "closed", "payload": None}])


def downgrade():
    op.drop_table("tickets")
