"""an index built outside the transaction, a value added to an enum

Revision ID: 0003
Revises: 0002
"""
from alembic import op

revision = "0003"
down_revision = "0002"


def upgrade():
    with op.get_context().autocommit_block():
        op.execute("ALTER TYPE status ADD VALUE IF NOT EXISTS 'archived'")
        op.create_index("ix_tickets_created", "tickets", ["created_at"], postgresql_concurrently=True)


def downgrade():
    pass
