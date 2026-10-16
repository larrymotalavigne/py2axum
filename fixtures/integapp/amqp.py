"""aio_pika: a task dispatcher publishing to a durable queue (one-shot connection), read back with queue.get()."""
import json
import os

import aio_pika
from aio_pika.exceptions import AMQPConnectionError, QueueEmpty
from fastapi import APIRouter

router = APIRouter(prefix="/amqp")

URL = os.environ.get("BROKER_DSN", "amqp://guest:guest@localhost:5672/")


class Dispatcher:
    async def send(self, task: str, args=(), kwargs=None, queue: str = "default") -> str:
        kwargs = kwargs or {}
        task_id = f"id-{task}"
        body = json.dumps([list(args), kwargs, {}]).encode("utf-8")
        message = aio_pika.Message(
            body=body,
            headers={"task": task, "n": 3, "big": 2**40, "f": 1.5, "flag": True, "none": None, "nested": {"a": [1, "x"]}},
            delivery_mode=aio_pika.DeliveryMode.PERSISTENT,
            message_id=task_id,
            content_type="application/json",
        )
        async with await aio_pika.connect_robust(URL) as conn:
            channel = await conn.channel()
            await channel.declare_queue(queue, durable=True)
            await channel.default_exchange.publish(message, routing_key=queue)
        return task_id


DISPATCHER = Dispatcher()


@router.post("/roundtrip/{task}")
async def roundtrip(task: str):
    queue_name = f"py2axum_test_{task}"
    sent = await DISPATCHER.send(task, args=(1, "é"), kwargs={"k": None}, queue=queue_name)
    async with await aio_pika.connect_robust(URL) as conn:
        channel = await conn.channel()
        queue = await channel.declare_queue(queue_name, durable=True)
        m = await queue.get(no_ack=True)
        empty = None
        try:
            await queue.get(no_ack=True)
        except QueueEmpty:
            empty = "QueueEmpty"
        missing = await queue.get(no_ack=True, fail=False)
    return {"sent": sent, "body": json.loads(m.body), "headers": m.headers, "id": m.message_id, "mode": m.delivery_mode,
            "ct": m.content_type, "rk": m.routing_key, "empty": empty, "missing": missing}


@router.get("/down")
async def down():
    try:
        await aio_pika.connect_robust("amqp://guest:guest@127.0.0.1:1/", fail_fast=True)
        return {"ok": True}
    except AMQPConnectionError as e:
        return {"error": type(e).__name__}
