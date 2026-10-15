"""after the refused revision: a second enum with its table

Revision ID: 0005
Revises: 0004
"""
import sqlalchemy as sa
from alembic import op

revision = "0005"
down_revision = "0004"


def upgrade():
    op.create_table("labels", sa.Column("id", sa.Integer, primary_key=True),
                    sa.Column("kind", sa.Enum("bug", "idea", name="label_kind")))
    op.add_column("tickets", sa.Column("label_id", sa.Integer, sa.ForeignKey("labels.id", ondelete="SET NULL")))


def downgrade():
    pass
