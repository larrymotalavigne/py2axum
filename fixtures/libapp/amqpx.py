"""aio_pika beyond publish/get: exchanges (direct, fanout, topic, headers), bindings, consumers (`queue.iterator()`,
`queue.consume(callback)`), `message.process()`, reject/nack/requeue, an RPC round trip through `reply_to` and
`correlation_id`, the broker's channel errors. Each route runs on queues and exchanges of its own (names made
from `tag`), deleted at the end."""
import asyncio
import os

import aio_pika
from aio_pika import ExchangeType, Message
from aio_pika.exceptions import ChannelClosed, ChannelNotFoundEntity, ChannelPreconditionFailed, QueueEmpty
from fastapi import APIRouter

router = APIRouter(prefix="/amqpx")

URL = os.environ.get("BROKER_DSN", "amqp://guest:guest@localhost:5672/")


def _seen(m):
    return {"body": m.body.decode(), "rk": m.routing_key, "exchange": m.exchange, "redelivered": m.redelivered,
            "cid": m.correlation_id, "reply_to": m.reply_to, "headers": m.headers, "priority": m.priority,
            "mode": int(m.delivery_mode), "has_id": m.message_id is not None, "type": m.type}


@router.post("/exchanges/{tag}")
async def exchanges(tag: str):
    out = {}
    async with await aio_pika.connect_robust(URL) as conn:
        ch = await conn.channel()
        for name, kind in (("direct", ExchangeType.DIRECT), ("fanout", ExchangeType.FANOUT), ("topic", ExchangeType.TOPIC),
                           ("direct", "direct")):
            x = await ch.declare_exchange(f"px_{tag}_{name}", kind)
            out[x.name] = repr(x)
        topic = await ch.get_exchange(f"px_{tag}_topic")
        q = await ch.declare_queue(f"pq_{tag}", durable=True)
        out["decl"] = [q.name, q.declaration_result.message_count, q.declaration_result.consumer_count]
        await q.bind(topic, routing_key="orders.*")
        await q.bind(f"px_{tag}_fanout")
        for rk, body in (("orders.new", "a"), ("orders.new.x", "skipped"), ("users.new", "skipped"), ("orders.old", "b")):
            await topic.publish(Message(body.encode(), headers={"rk": rk}), routing_key=rk)
        fan = await ch.get_exchange(f"px_{tag}_fanout")
        await fan.publish(Message(b"c", priority=3, correlation_id="k"), routing_key="whatever")
        got = []
        while (m := await q.get(no_ack=True, fail=False)) is not None:
            got.append(_seen(m))
        out["got"] = got
        out["unbind"] = type(await q.unbind(topic, routing_key="orders.*")).__name__
        await topic.publish(Message(b"after unbind"), routing_key="orders.new")
        out["after"] = await q.get(no_ack=True, fail=False)
        purge = await q.purge()
        out["purge"] = [type(purge).__name__, purge.message_count]
        out["delete"] = type(await q.delete()).__name__
        for x in out.copy():
            if x.startswith("px_"):
                await ch.exchange_delete(x)
    return out


@router.post("/consume/{tag}")
async def consume(tag: str, n: int = 3):
    async with await aio_pika.connect_robust(URL) as conn:
        ch = await conn.channel()
        await ch.set_qos(prefetch_count=2)
        q = await ch.declare_queue(f"pc_{tag}", durable=True)
        for i in range(n):
            await ch.default_exchange.publish(Message(f"m{i}".encode(), headers={"i": i}), routing_key=q.name)
        seen = []
        async with q.iterator() as it:
            async for m in it:
                async with m.process():
                    seen.append([m.body.decode(), m.headers["i"], m.processed if hasattr(m, "processed") else None])
                if len(seen) == n:
                    break
        timeout = None
        try:
            async with q.iterator(timeout=0.2) as it:
                async for m in it:
                    seen.append(m.body.decode())
        except asyncio.TimeoutError as e:
            timeout = [type(e).__name__, str(e)]
        await q.delete()
    return {"seen": seen, "timeout": timeout}


@router.post("/callback/{tag}")
async def callback(tag: str):
    async with await aio_pika.connect_robust(URL) as conn:
        ch = await conn.channel()
        q = await ch.declare_queue(f"pcb_{tag}", durable=True)
        got = []
        done = asyncio.Event()

        async def on_message(m):
            async with m.process():
                got.append(m.body.decode())
                if len(got) == 3:
                    done.set()

        consumer_tag = await q.consume(on_message)
        for i in range(3):
            await ch.default_exchange.publish(Message(f"c{i}".encode()), routing_key=q.name)
        await asyncio.wait_for(done.wait(), 5)
        cancel = type(await q.cancel(consumer_tag)).__name__
        await q.delete()
    return {"got": sorted(got), "tag": consumer_tag.startswith("ctag"), "cancel": cancel}


@router.post("/requeue/{tag}")
async def requeue(tag: str):
    async with await aio_pika.connect_robust(URL) as conn:
        ch = await conn.channel()
        q = await ch.declare_queue(f"pr_{tag}", durable=True)
        await ch.default_exchange.publish(Message(b"r"), routing_key=q.name)
        steps = []
        m = await q.get()
        steps.append(["first", m.redelivered])
        await m.reject(requeue=True)
        m = await q.get()
        steps.append(["after reject", m.redelivered])
        await m.nack(requeue=True)
        m = await q.get()
        try:
            async with m.process(requeue=True):
                raise ValueError("handler failed")
        except ValueError as e:
            steps.append(["process raised", str(e)])
        m = await q.get()
        steps.append(["requeued by process", m.redelivered])
        await m.ack()
        try:
            await m.ack()
        except Exception as e:
            steps.append(["double ack", type(e).__name__])
        steps.append(["empty", await q.get(fail=False)])
        try:
            await q.get()
        except QueueEmpty:
            steps.append(["QueueEmpty"])
        await q.delete()
    return steps


@router.post("/rpc/{tag}")
async def rpc(tag: str, x: int = 6):
    """a server consuming a request queue and answering to `reply_to`, a client waiting on its callback queue"""
    async with await aio_pika.connect_robust(URL) as conn:
        ch = await conn.channel()
        requests = await ch.declare_queue(f"prpc_{tag}", durable=True)

        async def serve(m):
            async with m.process():
                n = int(m.body)
                await ch.default_exchange.publish(Message(str(n * n).encode(), correlation_id=m.correlation_id),
                                                  routing_key=m.reply_to)

        server_tag = await requests.consume(serve)
        callback_queue = await ch.declare_queue(exclusive=True)
        pending: dict[str, asyncio.Future] = {}

        async def on_reply(m):
            async with m.process():
                fut = pending.pop(m.correlation_id, None)
                if fut is not None:
                    fut.set_result(int(m.body))

        client_tag = await callback_queue.consume(on_reply)
        results = []
        for i in range(3):
            cid = f"{tag}-{i}"
            fut = asyncio.get_running_loop().create_future()
            pending[cid] = fut
            await ch.default_exchange.publish(Message(str(x + i).encode(), correlation_id=cid, reply_to=callback_queue.name),
                                              routing_key=requests.name)
            results.append(await asyncio.wait_for(fut, 5))
        await requests.cancel(server_tag)
        await callback_queue.cancel(client_tag)
        await requests.delete()
    return {"results": results, "pending": len(pending)}


@router.post("/errors/{tag}")
async def errors(tag: str):
    out = []
    conn = await aio_pika.connect(URL)
    try:
        ch = await conn.channel()
        try:
            await ch.declare_exchange(f"pe_missing_{tag}", passive=True)
        except ChannelNotFoundEntity as e:
            out.append(["passive", type(e).__name__, str(e), isinstance(e, ChannelClosed)])
        out.append(["closed", ch.is_closed])
        ch = await conn.channel()
        await ch.declare_queue(f"pe_{tag}", durable=True)
        try:
            await ch.declare_queue(f"pe_{tag}", durable=False)
        except ChannelPreconditionFailed as e:
            out.append(["precondition", type(e).__name__, str(e)])
        ch = await conn.channel()
        try:
            await ch.declare_queue(f"pe_absent_{tag}", passive=True)
        except ChannelNotFoundEntity as e:
            out.append(["queue passive", str(e)])
        ch = await conn.channel()
        x = await ch.declare_exchange(f"pe_x_{tag}", ExchangeType.DIRECT, durable=True)
        try:
            await ch.declare_exchange(f"pe_x_{tag}", ExchangeType.FANOUT, durable=True)
        except ChannelPreconditionFailed as e:
            out.append(["exchange type", str(e)])
        ch = await conn.channel()
        await ch.exchange_delete(x.name)
        await ch.queue_delete(f"pe_{tag}")
    finally:
        await conn.close()
    return out
