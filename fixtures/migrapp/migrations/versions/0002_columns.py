"""columns: added, altered, an enum type made by op.execute, a check constraint

Revision ID: 0002
Revises: 0001
"""
import sqlalchemy as sa
from alembic import op
from sqlalchemy.dialects import postgresql

revision = "0002"
down_revision = "0001"


def upgrade():
    op.execute("CREATE TYPE priority AS ENUM ('low', 'high')")
    op.add_column("tickets", sa.Column("priority", postgresql.ENUM("low", "high", name="priority", create_type=False),
                                       nullable=True))
    op.alter_column("tickets", "title", type_=sa.String(300), existing_type=sa.String(200))
    op.create_check_constraint("ck_title_len", "tickets", "length(title) > 0")
    op.execute(sa.text("UPDATE tickets SET priority = 'high' WHERE id = 1"))


def downgrade():
    pass
