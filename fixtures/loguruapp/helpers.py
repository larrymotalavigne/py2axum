from loguru import logger


class Box:
    def __init__(self, v):
        self.v = v

    def __str__(self):
        return f"Box({self.v})"


class Service:
    def run(self, what: str) -> None:
        logger.info("Service.run: {}", what)


def nested_log(what: str) -> None:
    def inner():
        logger.warning("inner of nested_log: " + what)
    inner()
    logger.info("nested_log: {}", what)
