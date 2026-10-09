from decimal import Decimal
from typing import Literal

from fastapi import APIRouter
from pydantic import BaseModel, ConfigDict, EmailStr, Field, computed_field, field_validator, model_validator

router = APIRouter(prefix="/models", tags=["models"])


class Address(BaseModel):
    city: str
    country: str = Field(min_length=2, max_length=2, description="ISO 3166 code")


class Customer(BaseModel):
    model_config = ConfigDict(str_strip_whitespace=True, populate_by_name=True)

    email: EmailStr
    name: str = Field(min_length=1, max_length=40)
    plan: Literal["free", "pro"] = "free"
    tags: list[str] = []
    addresses: list[Address] = []
    credit: Decimal = Field(default=Decimal("0"), max_digits=8, decimal_places=2)
    referrer_id: int | None = Field(default=None, alias="referrerId")

    @field_validator("tags")
    @classmethod
    def normalize_tags(cls, tags: list[str]) -> list[str]:
        return sorted({tag.lower() for tag in tags})

    @model_validator(mode="after")
    def pro_needs_an_address(self):
        if self.plan == "pro" and not self.addresses:
            raise ValueError("a pro customer needs an address")
        return self

    @computed_field
    @property
    def display_name(self) -> str:
        return f"{self.name} <{self.email}>"


@router.post("/customers")
async def create_customer(customer: Customer) -> Customer:
    return customer


@router.post("/customers/dump")
async def dump_customer(customer: Customer):
    return {
        "python": customer.model_dump(exclude={"addresses"}),
        "json": customer.model_dump(mode="json", by_alias=True, exclude_none=True),
        "fields_set": sorted(customer.model_fields_set),
    }
