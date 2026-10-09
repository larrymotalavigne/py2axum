from fastapi import APIRouter, BackgroundTasks

router = APIRouter(prefix="/background", tags=["background"])

NOTIFICATIONS: list[str] = []


def write_notification(email: str, message: str = ""):
    NOTIFICATIONS.append(f"notification for {email}: {message}")


@router.post("/send-notification/{email}")
async def send_notification(email: str, background_tasks: BackgroundTasks):
    background_tasks.add_task(write_notification, email, message="some notification")
    return {"message": "Notification sent in the background"}


@router.get("/notifications")
async def read_notifications():
    return NOTIFICATIONS
