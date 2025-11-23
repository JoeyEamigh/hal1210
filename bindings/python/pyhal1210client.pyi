from __future__ import annotations

from typing import Awaitable, Callable, Literal, Optional, Tuple, TypedDict, Union

RgbColor = Tuple[int, int, int]


class SetStaticColor(TypedDict):
    command: Literal["setStaticColor"]
    args: RgbColor


class SetStripState(TypedDict):
    command: Literal["setStripState"]
    args: str


class FadeIn(TypedDict):
    command: Literal["fadeIn"]
    args: str


class FadeOut(TypedDict):
    command: Literal["fadeOut"]


class Rainbow(TypedDict):
    command: Literal["rainbow"]


class Breathing(TypedDict):
    command: Literal["breathing"]
    args: RgbColor


LedCommand = Union[SetStaticColor, SetStripState, FadeIn, FadeOut, Rainbow, Breathing]


class LedMessage(TypedDict):
    type: Literal["led"]
    data: LedCommand


class ManualModePayload(TypedDict):
    enabled: bool


class SetManualModeMessage(TypedDict):
    type: Literal["setManualMode"]
    data: ManualModePayload


class GetManualModeMessage(TypedDict):
    type: Literal["getManualMode"]


MessageToServerData = Union[LedMessage, SetManualModeMessage, GetManualModeMessage]


class MessageBase(TypedDict):
    id: str


class AckMessage(MessageBase):
    type: Literal["ack"]


class NackPayload(TypedDict):
    reason: str


class NackMessage(MessageBase):
    type: Literal["nack"]
    data: NackPayload


class ManualModeState(TypedDict):
    enabled: bool


class ManualModeMessage(MessageBase):
    type: Literal["manualMode"]
    data: ManualModeState


MessageToClient = Union[AckMessage, NackMessage, ManualModeMessage]


MessageCallback = Callable[[MessageToClient], None]


class Hal1210Client:
    @classmethod
    def connect(cls) -> "Hal1210Client": ...

    def send(self, message: MessageToServerData) -> str: ...

    def next_message(self) -> Optional[MessageToClient]: ...

    def next_message_async(self) -> Awaitable[Optional[MessageToClient]]: ...

    def on_message(self, callback: MessageCallback) -> None: ...

    def cancel(self) -> None: ...


__all__ = [
    "Hal1210Client",
    "LedCommand",
    "MessageToServerData",
    "MessageToClient",
    "MessageCallback",
    "RgbColor",
]
