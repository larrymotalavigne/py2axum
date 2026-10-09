import secrets
from typing import Annotated

from fastapi import APIRouter, Depends, HTTPException, status
from fastapi.security import HTTPBasic, HTTPBasicCredentials, OAuth2PasswordBearer, OAuth2PasswordRequestForm
from pydantic import BaseModel

router = APIRouter(prefix="/security", tags=["security"])

oauth2_scheme = OAuth2PasswordBearer(tokenUrl="security/token")
basic = HTTPBasic()

# a real application stores password hashes (bcrypt) and signs tokens (PyJWT): see the bookshelf example
USERS = {
    "alice": {"username": "alice", "full_name": "Alice Wonderson", "password": "secret", "disabled": False},
    "bob": {"username": "bob", "full_name": "Bob Builder", "password": "builder", "disabled": True},
}


class User(BaseModel):
    username: str
    full_name: str | None = None
    disabled: bool = False


@router.post("/token")
async def login(form: Annotated[OAuth2PasswordRequestForm, Depends()]):
    user = USERS.get(form.username)
    if user is None or not secrets.compare_digest(form.password, user["password"]):
        raise HTTPException(status_code=400, detail="Incorrect username or password")
    return {"access_token": user["username"], "token_type": "bearer"}


async def get_current_user(token: Annotated[str, Depends(oauth2_scheme)]) -> User:
    user = USERS.get(token)
    if user is None:
        raise HTTPException(
            status_code=status.HTTP_401_UNAUTHORIZED,
            detail="Invalid authentication credentials",
            headers={"WWW-Authenticate": "Bearer"},
        )
    return User(**user)


async def get_active_user(user: Annotated[User, Depends(get_current_user)]) -> User:
    if user.disabled:
        raise HTTPException(status_code=400, detail="Inactive user")
    return user


@router.get("/me")
async def read_me(user: Annotated[User, Depends(get_active_user)]) -> User:
    return user


@router.get("/basic")
async def read_basic(credentials: Annotated[HTTPBasicCredentials, Depends(basic)]):
    user = USERS.get(credentials.username)
    if user is None or not secrets.compare_digest(credentials.password, user["password"]):
        raise HTTPException(
            status_code=status.HTTP_401_UNAUTHORIZED,
            detail="Incorrect username or password",
            headers={"WWW-Authenticate": "Basic"},
        )
    return {"username": credentials.username}
