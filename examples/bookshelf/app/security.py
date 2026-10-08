"""Passwords (bcrypt) and access tokens (PyJWT, HS256), read by a Bearer dependency."""
import os
from datetime import UTC, datetime, timedelta
from typing import Annotated

import bcrypt
import jwt
from fastapi import Depends, HTTPException, status
from fastapi.security import HTTPAuthorizationCredentials, HTTPBearer

from .db import DbDep
from .models import User

SECRET_KEY = os.environ.get("BOOKSHELF_SECRET", "change-me-in-production-0123456789")
ALGORITHM = "HS256"
TOKEN_MINUTES = int(os.environ.get("BOOKSHELF_TOKEN_MINUTES", "30"))
# bcrypt's cost factor: 12 is a sensible production value, tests lower it
BCRYPT_ROUNDS = int(os.environ.get("BOOKSHELF_BCRYPT_ROUNDS", "12"))

bearer = HTTPBearer(auto_error=False)


def hash_password(password: str) -> str:
    return bcrypt.hashpw(password.encode(), bcrypt.gensalt(rounds=BCRYPT_ROUNDS)).decode()


def verify_password(password: str, password_hash: str) -> bool:
    return bcrypt.checkpw(password.encode(), password_hash.encode())


def create_access_token(user_id: int) -> str:
    now = datetime.now(UTC)
    claims = {"sub": str(user_id), "iat": now, "exp": now + timedelta(minutes=TOKEN_MINUTES)}
    return jwt.encode(claims, SECRET_KEY, algorithm=ALGORITHM)


def _unauthorized(detail: str) -> HTTPException:
    return HTTPException(status_code=status.HTTP_401_UNAUTHORIZED, detail=detail, headers={"WWW-Authenticate": "Bearer"})


def user_id_from_token(token: str) -> int | None:
    """The user id a valid token carries, None for an invalid or expired one."""
    try:
        claims = jwt.decode(token, SECRET_KEY, algorithms=[ALGORITHM], options={"require": ["exp", "sub"]})
    except jwt.InvalidTokenError:
        return None
    sub = claims["sub"]
    return int(sub) if sub.isdigit() else None


async def current_user(
    db: DbDep, credentials: Annotated[HTTPAuthorizationCredentials | None, Depends(bearer)]
) -> User:
    if credentials is None:
        raise _unauthorized("not authenticated")
    try:
        claims = jwt.decode(credentials.credentials, SECRET_KEY, algorithms=[ALGORITHM], options={"require": ["exp", "sub"]})
    except jwt.ExpiredSignatureError:
        raise _unauthorized("token expired")
    except jwt.InvalidTokenError as exc:
        raise _unauthorized(f"invalid token: {exc}")
    user = await db.get(User, int(claims["sub"])) if claims["sub"].isdigit() else None
    if user is None:
        raise _unauthorized("unknown user")
    return user


CurrentUser = Annotated[User, Depends(current_user)]
