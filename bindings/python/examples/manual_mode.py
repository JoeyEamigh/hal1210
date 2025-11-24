"""Manual-mode workflow demo for the hal1210 Python bindings.

Run with: uv run python examples/manual_mode.py
"""

from __future__ import annotations

import asyncio
from typing import TYPE_CHECKING, Any, cast

from pyhal1210client import Hal1210Client

if TYPE_CHECKING:
    from pyhal1210client import ManualModeMessage, MessageToClient, MessageToServerData, NackMessage
else:  # pragma: no cover - runtime only
    ManualModeMessage = Any  # type: ignore[assignment]
    MessageToClient = Any
    MessageToServerData = Any
    NackMessage = Any  # type: ignore[assignment]

GET_MANUAL_MODE: MessageToServerData = {"type": "getManualMode"}
RAINBOW_COMMAND: MessageToServerData = {"type": "led", "data": {"command": "rainbow"}}
TIMEOUT_SECONDS = 2.0
RAINBOW_DURATION_SECONDS = 10.0


async def wait_for_next_message(client: Hal1210Client, timeout: float) -> MessageToClient:
    try:
        response = await asyncio.wait_for(asyncio.to_thread(client.next_message), timeout=timeout)
    except asyncio.TimeoutError as exc:
        raise RuntimeError("Timed out waiting for the hal1210 daemon. Is it running?") from exc

    if response is None:
        raise RuntimeError("Daemon closed the connection before responding.")

    return response


async def wait_for_type(
    client: Hal1210Client,
    expected_type: str,
    timeout: float,
) -> MessageToClient:
    loop = asyncio.get_running_loop()
    deadline = loop.time() + timeout
    while True:
        remaining = deadline - loop.time()
        if remaining <= 0:
            raise RuntimeError(f"Timed out waiting for {expected_type} message.")
        message = await wait_for_next_message(client, remaining)
        if message["type"] == expected_type:
            return message
        print(
            f"Received {message['type']} ({message['id']}) while waiting for {expected_type}; continuing...",
        )


async def wait_for_ack(client: Hal1210Client, expected_id: str) -> None:
    loop = asyncio.get_running_loop()
    deadline = loop.time() + TIMEOUT_SECONDS
    while True:
        remaining = deadline - loop.time()
        if remaining <= 0:
            raise RuntimeError(f"Timed out waiting for ack {expected_id}.")
        message = await wait_for_next_message(client, remaining)
        msg_type = message["type"]
        msg_id = message["id"]
        if msg_type == "ack" and msg_id == expected_id:
            print(f"Daemon acknowledged {expected_id}.")
            return
        if msg_type == "nack" and msg_id == expected_id:
            nack = cast(NackMessage, message)
            reason = nack["data"]["reason"]
            raise RuntimeError(f"Daemon rejected {expected_id}: {reason}")
        print(f"Received {msg_type} ({msg_id}) while waiting for ack {expected_id}; continuing...")


async def request_manual_mode_status(client: Hal1210Client, label: str) -> bool:
    message_id = client.send(GET_MANUAL_MODE)
    print(f"Sent manual mode request ({label}) (message id: {message_id})")
    response = cast(ManualModeMessage, await wait_for_type(client, "manualMode", TIMEOUT_SECONDS))
    enabled = bool(response["data"]["enabled"])
    state = "ENABLED" if enabled else "DISABLED"
    print(f"{label}: daemon reports manual mode {state}.")
    return enabled


async def set_manual_mode(client: Hal1210Client, enabled: bool) -> None:
    payload: MessageToServerData = {"type": "setManualMode", "data": {"enabled": enabled}}
    message_id = client.send(payload)
    print(f"Sent {'enable' if enabled else 'disable'} manual mode request (message id: {message_id}).")
    await wait_for_ack(client, message_id)


async def start_rainbow_effect(client: Hal1210Client) -> None:
    message_id = client.send(RAINBOW_COMMAND)
    print(f"Sent rainbow command (message id: {message_id}).")
    await wait_for_ack(client, message_id)


async def run_manual_mode_workflow(client: Hal1210Client) -> None:
    await request_manual_mode_status(client, "Initial status")

    await set_manual_mode(client, True)
    enabled = await request_manual_mode_status(client, "Post-enable status")
    if not enabled:
        raise RuntimeError("Daemon did not report manual mode as enabled.")

    await start_rainbow_effect(client)
    print(f"Rainbow effect running for {RAINBOW_DURATION_SECONDS:.0f} seconds...")
    await asyncio.sleep(RAINBOW_DURATION_SECONDS)

    await set_manual_mode(client, False)
    print("Manual mode disabled; disconnecting.")


async def main() -> None:
    client = Hal1210Client.connect(enable_tracing=True)
    try:
        await run_manual_mode_workflow(client)
    finally:
        client.cancel()


if __name__ == "__main__":
    asyncio.run(main())
