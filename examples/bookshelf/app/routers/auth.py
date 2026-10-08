from fastapi import APIRouter, HTTPException, status
from sqlalchemy import select
from sqlalchemy.exc import IntegrityError

from ..db import DbDep
from ..models import User
from ..schemas import LoginIn, Token, UserCreate, UserOut
from ..security import TOKEN_MINUTES, CurrentUser, create_access_token, hash_password, verify_password

router = APIRouter(tags=["auth"])


@router.post("/auth/register", response_model=UserOut, status_code=status.HTTP_201_CREATED)
async def register(payload: UserCreate, db: DbDep):
    user = User(email=payload.email.lower(), display_name=payload.display_name, password_hash=hash_password(payload.password))
    db.add(user)
    try:
        await db.flush()
    except IntegrityError:
        raise HTTPException(status_code=status.HTTP_409_CONFLICT, detail="e-mail address already registered")
    await db.refresh(user)
    return user


@router.post("/auth/login", response_model=Token)
async def login(payload: LoginIn, db: DbDep):
    user = (await db.execute(select(User).where(User.email == payload.email.lower()))).scalar_one_or_none()
    if user is None or not verify_password(payload.password, user.password_hash):
        raise HTTPException(status_code=status.HTTP_401_UNAUTHORIZED, detail="wrong e-mail address or password")
    return Token(access_token=create_access_token(user.id), expires_in=TOKEN_MINUTES * 60)


@router.get("/me", response_model=UserOut)
async def me(user: CurrentUser):
    return user
