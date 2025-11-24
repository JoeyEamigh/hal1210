from __future__ import annotations

from typing import Awaitable, Callable, Literal, NotRequired, Optional, Tuple, TypedDict, Union

RgbColor = Tuple[int, int, int]


class SetStaticColor(TypedDict):
    command: Literal["setStaticColor"]
    args: RgbColor


class SetStripState(TypedDict):
    command: Literal["setStripState"]
    args: str


class FadeIn(TypedDict):
    command: Literal["fadeIn"]
    args: Union[str, "FadeInArgs"]


class FadeOut(TypedDict):
    command: Literal["fadeOut"]
    args: NotRequired["FadeOutArgs"]


class FadeInArgs(TypedDict):
    state: str
    durationMs: NotRequired[int]


class FadeOutArgs(TypedDict):
    durationMs: NotRequired[int]


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


class IdleInhibitPayload(TypedDict):
    enabled: bool
    timeoutMs: NotRequired[int]


class SetIdleInhibitMessage(TypedDict):
    type: Literal["setIdleInhibit"]
    data: IdleInhibitPayload


class GetIdleInhibitMessage(TypedDict):
    type: Literal["getIdleInhibit"]


MessageToServerData = Union[
    LedMessage,
    SetManualModeMessage,
    GetManualModeMessage,
    SetIdleInhibitMessage,
    GetIdleInhibitMessage,
]


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


class IdleInhibitState(TypedDict):
    enabled: bool
    timeoutMs: NotRequired[int]


class IdleInhibitMessage(MessageBase):
    type: Literal["idleInhibit"]
    data: IdleInhibitState


MessageToClient = Union[AckMessage, NackMessage, ManualModeMessage, IdleInhibitMessage]


MessageCallback = Callable[[MessageToClient], None]


class Hal1210Client:
    @classmethod
    def connect(cls, enable_tracing: bool = ...) -> "Hal1210Client": ...

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
